//! Overlay stack & HelpOverlay (`?`) for standalone `@shinick/notion` TUI.

pub mod attachments;
pub mod command_palette;
pub mod confirm;
pub mod context_menu;
pub mod file_picker;
pub mod image_preview;
pub mod links;
pub mod notion_form;
pub mod path_input;
pub mod search;
pub mod settings;
pub mod workspace_switcher;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
    Frame,
};

use crate::ui::{
    action::Action,
    app::{AppState, UiState},
    hitmap::{HitMap, HitTarget},
    overlays::{
        attachments::AttachmentsState, command_palette::CommandPalette, confirm::ConfirmDialog,
        context_menu::ContextMenu, file_picker::FilePickerOverlay,
        image_preview::ImagePreviewOverlay, links::LinkExtractorOverlay,
        notion_form::NotionFormOverlay, path_input::PathInputState, search::SearchOverlay,
        settings::SettingsState, workspace_switcher::WorkspaceSwitcherOverlay,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayKind {
    ContextMenu,
    CommandPalette,
    WorkspaceSwitcher,
    Settings,
    Search,
    Attachments,
    PathInput,
    FilePicker,
    Links,
    NotionForm,
    ImagePreview,
    Help,
    Confirm,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayOutcome {
    Handled,
    Close,
    Dispatch(Action),
    CloseAndDispatch(Action),
    NotHandled,
}

#[derive(Debug, Clone)]
pub enum Overlay {
    ContextMenu(ContextMenu),
    CommandPalette(CommandPalette),
    WorkspaceSwitcher(WorkspaceSwitcherOverlay),
    Settings(SettingsState),
    Search(SearchOverlay),
    Attachments(AttachmentsState),
    PathInput(PathInputState),
    FilePicker(FilePickerOverlay),
    Links(LinkExtractorOverlay),
    NotionForm(NotionFormOverlay),
    ImagePreview(ImagePreviewOverlay),
    Help(HelpOverlay),
    Confirm(ConfirmDialog),
}

impl Overlay {
    pub fn kind(&self) -> OverlayKind {
        match self {
            Overlay::ContextMenu(_) => OverlayKind::ContextMenu,
            Overlay::CommandPalette(_) => OverlayKind::CommandPalette,
            Overlay::WorkspaceSwitcher(_) => OverlayKind::WorkspaceSwitcher,
            Overlay::Settings(_) => OverlayKind::Settings,
            Overlay::Search(_) => OverlayKind::Search,
            Overlay::Attachments(_) => OverlayKind::Attachments,
            Overlay::PathInput(_) => OverlayKind::PathInput,
            Overlay::FilePicker(_) => OverlayKind::FilePicker,
            Overlay::Links(_) => OverlayKind::Links,
            Overlay::NotionForm(_) => OverlayKind::NotionForm,
            Overlay::ImagePreview(_) => OverlayKind::ImagePreview,
            Overlay::Help(_) => OverlayKind::Help,
            Overlay::Confirm(_) => OverlayKind::Confirm,
        }
    }

    pub fn accepts_text(&self) -> bool {
        match self {
            Overlay::Search(_)
            | Overlay::CommandPalette(_)
            | Overlay::PathInput(_)
            | Overlay::FilePicker(_) => true,
            Overlay::NotionForm(form) => form.accepts_text(),
            _ => false,
        }
    }

    pub fn context_menu(menu: ContextMenu) -> Self {
        Overlay::ContextMenu(menu)
    }

    pub fn command_palette() -> Self {
        Overlay::CommandPalette(CommandPalette::new())
    }

    pub fn workspace_switcher() -> Self {
        Overlay::WorkspaceSwitcher(WorkspaceSwitcherOverlay::default())
    }

    pub fn settings() -> Self {
        Overlay::Settings(SettingsState::default())
    }

    pub fn search() -> Self {
        Overlay::Search(SearchOverlay::default())
    }

    pub fn attachments() -> Self {
        Overlay::Attachments(AttachmentsState::default())
    }

    pub fn path_input() -> Self {
        Overlay::PathInput(PathInputState::new())
    }

    pub fn file_picker() -> Self {
        Overlay::FilePicker(FilePickerOverlay::new())
    }

    pub fn links(overlay: LinkExtractorOverlay) -> Self {
        Overlay::Links(overlay)
    }

    pub fn notion_form(form: NotionFormOverlay) -> Self {
        Overlay::NotionForm(form)
    }

    pub fn help() -> Self {
        Overlay::Help(HelpOverlay)
    }

    pub fn confirm(message: impl Into<String>, on_confirm: Option<Action>) -> Self {
        Overlay::Confirm(ConfirmDialog::new(message, on_confirm))
    }

    pub fn handle(&mut self, action: &Action, app: &AppState, ui: &UiState) -> OverlayOutcome {
        match self {
            Overlay::ContextMenu(menu) => match action {
                Action::MoveUp => {
                    menu.prev();
                    OverlayOutcome::Handled
                }
                Action::MoveDown => {
                    menu.next();
                    OverlayOutcome::Handled
                }
                Action::Activate => menu.activate(),
                Action::CloseOverlay => OverlayOutcome::Close,
                Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
                _ => OverlayOutcome::Handled,
            },
            Overlay::CommandPalette(palette) => match action {
                Action::Tab => palette.handle(&Action::MoveDown, app, ui),
                Action::PrevFocus => palette.handle(&Action::MoveUp, app, ui),
                other => palette.handle(other, app, ui),
            },
            Overlay::WorkspaceSwitcher(ws) => ws.handle(action, app),
            Overlay::Settings(settings) => match action {
                Action::Tab => settings.handle(&Action::MoveDown, app),
                Action::PrevFocus => settings.handle(&Action::MoveUp, app),
                other => settings.handle(other, app),
            },
            Overlay::Search(search) => search.handle(action, app),
            Overlay::Attachments(tray) => tray.handle(action, app),
            Overlay::PathInput(input) => input.handle(action),
            Overlay::FilePicker(picker) => picker.handle(action),
            Overlay::Links(links) => links.handle(action),
            Overlay::NotionForm(form) => form.handle(action),
            Overlay::ImagePreview(preview) => preview.handle(action),
            Overlay::Help(_) => match action {
                Action::CloseOverlay | Action::Activate | Action::Input('q') | Action::Input('?') => {
                    OverlayOutcome::Close
                }
                Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
                _ => OverlayOutcome::Handled,
            },
            Overlay::Confirm(dialog) => dialog.handle(action),
        }
    }

    pub fn select(&mut self, index: usize) {
        match self {
            Overlay::ContextMenu(menu) => menu.select(index),
            Overlay::CommandPalette(palette) => palette.select(index),
            Overlay::WorkspaceSwitcher(ws) => ws.select(index),
            Overlay::Settings(settings) => settings.select(index),
            Overlay::Search(search) => search.select(index),
            Overlay::Attachments(tray) => tray.select(index),
            Overlay::FilePicker(picker) => picker.select(index),
            Overlay::Links(links) => links.select(index),
            Overlay::NotionForm(form) => form.select(index),
            Overlay::ImagePreview(_)
            | Overlay::PathInput(_)
            | Overlay::Help(_)
            | Overlay::Confirm(_) => {}
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
        match self {
            Overlay::ContextMenu(menu) => menu.draw(frame, area, ui, hitmap),
            Overlay::CommandPalette(palette) => palette.draw(frame, area, app, ui, hitmap),
            Overlay::WorkspaceSwitcher(ws) => ws.draw(frame, area, app, ui, hitmap),
            Overlay::Settings(settings) => settings.draw(frame, area, app, ui, hitmap),
            Overlay::Search(search) => search.draw(frame, area, app, ui, hitmap),
            Overlay::Attachments(tray) => tray.draw(frame, area, app, ui, hitmap),
            Overlay::PathInput(input) => input.draw(frame, area, ui, hitmap),
            Overlay::FilePicker(picker) => picker.draw(frame, area, ui, hitmap),
            Overlay::Links(links) => links.draw(frame, area, ui, hitmap),
            Overlay::NotionForm(form) => form.draw(frame, area, ui, hitmap),
            Overlay::ImagePreview(preview) => preview.draw(frame, area, app, ui, hitmap),
            Overlay::Help(help) => help.draw(frame, area, ui, hitmap),
            Overlay::Confirm(dialog) => dialog.draw(frame, area, ui, hitmap),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct HelpOverlay;

impl HelpOverlay {
    pub fn draw(&self, frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
        let theme = &ui.theme;
        let rect = centered_rect(area, 80, 26);
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                " @shinick/notion · Keyboard Shortcuts · Esc Close ",
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        let lines = vec![
            Line::from(Span::styled("Workspace & Navigation", theme.title(true))),
            Line::from("  Ctrl+K / Ctrl+P       Notion Quick Find & Command Palette"),
            Line::from("  Ctrl+S                Workspace Switcher"),
            Line::from("  [ / Ctrl+B            Collapse / Expand Left Sidebar"),
            Line::from("  Tab / Shift+Tab       Cycle focus (Sidebar / Canvas / Comments)"),
            Line::from("  j / k / Up / Down     Navigate tree items, blocks, rows, or cards"),
            Line::from("  Ctrl+D / Ctrl+U       Smooth half-page scroll down / up"),
            Line::from("  Backspace / Alt+Left  Navigate back up breadcrumb stack"),
            Line::from(""),
            Line::from(Span::styled("Page Canvas & Authoring", theme.title(true))),
            Line::from("  w                     Toggle Centered editorial width vs Full width"),
            Line::from("  Space / x             Toggle to-do checkbox or expand/collapse toggle"),
            Line::from("  Enter                 Expand toggle, open child page/DB, or open DB card"),
            Line::from("  a                     Slash Quick Append (/h1, /h2, /todo, /callout, /code)"),
            Line::from("  e / n / f             Edit in $EDITOR · New Child Page · Toggle Favorite ★"),
            Line::from("  C / u / h             Comments Drawer · Link Extractor · Toggle [href]"),
            Line::from("  v / Alt+V / A         HD Image Preview · Media & Files tray"),
            Line::from(""),
            Line::from(Span::styled("Database Views (Table / Board / List)", theme.title(true))),
            Line::from("  1 / 2 / 3             Switch Table · Kanban Board · List view"),
            Line::from("  h / l / m             Board: switch columns · move card to next status"),
            Line::from("  f / s / n             Filter rows · Cycle Sort · New Database Item"),
            Line::from("  Ctrl+F / Ctrl+V / ,   File Picker (z zoxide) · Paste · Settings · Ctrl+Q Quit"),
        ];
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }
}

pub fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(area.height.saturating_sub(h) / 2),
            Constraint::Length(h),
            Constraint::Min(0),
        ])
        .split(area);
    let horiz = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(area.width.saturating_sub(w) / 2),
            Constraint::Length(w),
            Constraint::Min(0),
        ])
        .split(vert[1]);
    horiz[1]
}
