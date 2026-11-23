//! Overlay stack & HelpOverlay (`?`) for standalone `@bitefoundry/notion` TUI.

pub mod attachments;
pub mod command_palette;
pub mod confirm;
pub mod context_menu;
pub mod file_picker;
pub mod html_preview;
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
        html_preview::HtmlPreviewOverlay, image_preview::ImagePreviewOverlay,
        links::LinkExtractorOverlay, notion_form::NotionFormOverlay, path_input::PathInputState,
        search::SearchOverlay, settings::SettingsState,
        workspace_switcher::WorkspaceSwitcherOverlay,
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
    HtmlPreview,
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

#[derive(Debug)]
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
    HtmlPreview(HtmlPreviewOverlay),
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
            Overlay::HtmlPreview(_) => OverlayKind::HtmlPreview,
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
            Overlay::HtmlPreview(html) => html.accepts_text(),
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

    pub fn html_preview(title: impl Into<String>, html: String) -> Self {
        Overlay::HtmlPreview(HtmlPreviewOverlay::new(title, html))
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
                Action::MoveUp | Action::Input('k') => {
                    menu.prev();
                    OverlayOutcome::Handled
                }
                Action::MoveDown | Action::Input('j') => {
                    menu.next();
                    OverlayOutcome::Handled
                }
                Action::PageUp => {
                    menu.scroll_by(-5);
                    OverlayOutcome::Handled
                }
                Action::PageDown => {
                    menu.scroll_by(5);
                    OverlayOutcome::Handled
                }
                Action::CursorHome | Action::ScrollTop | Action::Input('g') => {
                    menu.selected = 0;
                    OverlayOutcome::Handled
                }
                Action::CursorEnd | Action::ScrollBottom | Action::Input('G') => {
                    menu.selected = menu.items.len().saturating_sub(1);
                    OverlayOutcome::Handled
                }
                Action::MouseScroll { delta, .. } => {
                    menu.scroll_by(*delta as isize);
                    OverlayOutcome::Handled
                }
                Action::Activate | Action::OverlaySubmit => menu.activate(),
                Action::CloseOverlay | Action::Input('q') => OverlayOutcome::Close,
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
            Overlay::HtmlPreview(html) => html.handle(action),
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
            | Overlay::HtmlPreview(_)
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
    ) -> Option<(u16, u16)> {
        match self {
            Overlay::ContextMenu(menu) => {
                menu.draw(frame, area, ui, hitmap);
                None
            }
            Overlay::CommandPalette(palette) => {
                palette.draw(frame, area, app, ui, hitmap);
                None
            }
            Overlay::WorkspaceSwitcher(ws) => {
                ws.draw(frame, area, app, ui, hitmap);
                None
            }
            Overlay::Settings(settings) => {
                settings.draw(frame, area, app, ui, hitmap);
                None
            }
            Overlay::Search(search) => {
                search.draw(frame, area, app, ui, hitmap);
                None
            }
            Overlay::Attachments(tray) => {
                tray.draw(frame, area, app, ui, hitmap);
                None
            }
            Overlay::PathInput(input) => {
                input.draw(frame, area, ui, hitmap);
                None
            }
            Overlay::FilePicker(picker) => {
                picker.draw(frame, area, ui, hitmap);
                None
            }
            Overlay::Links(links) => {
                links.draw(frame, area, ui, hitmap);
                None
            }
            Overlay::NotionForm(form) => {
                form.draw(frame, area, ui, hitmap);
                None
            }
            Overlay::ImagePreview(preview) => {
                preview.draw(frame, area, app, ui, hitmap);
                None
            }
            Overlay::HtmlPreview(html) => {
                html.draw(frame, area, app, ui, hitmap);
                None
            }
            Overlay::Help(help) => {
                help.draw(frame, area, ui, hitmap);
                None
            }
            Overlay::Confirm(dialog) => {
                dialog.draw(frame, area, ui, hitmap);
                None
            }
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
                " @bitefoundry/notion · Keyboard Shortcuts · Esc Close ",
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
            Line::from("  e / Ctrl+E / n / f    Edit Page (Ctrl+S Save, Esc Done/Discard) · $EDITOR · New · Fav ★"),
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

/// Moves a list selection by `delta` rows, clamping cleanly to `0..len.saturating_sub(1)`
/// without wrapping around.
pub(crate) fn scroll_selection_clamped(selected: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    let max_idx = len - 1;
    let cur = selected.min(max_idx);
    if delta < 0 {
        cur.saturating_sub(delta.unsigned_abs())
    } else {
        cur.saturating_add(delta as usize).min(max_idx)
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tokio::sync::{mpsc, watch};

    use super::*;
    use crate::{
        auth::AuthManager,
        config::{AppConfig, WorkspaceProfile},
        models::{DatabaseViewMode, NotionMediaRef, NotionPage, NotionPageDetail},
        notion::NotionClient,
        storage::Storage,
        sync::{FocusTarget, SyncController},
        ui::{
            app::Focus,
            event::key_to_action,
            overlays::{
                context_menu::ContextTarget,
                file_picker::FilePickerMode,
                links::ExtractedLink,
            },
        },
    };

    fn test_app_and_ui() -> (AppState, UiState) {
        let cfg = AppConfig {
            workspaces: (0..5)
                .map(|i| WorkspaceProfile {
                    name: format!("ws-{i}"),
                    workspace_id: format!("wid-{i}"),
                    workspace_name: format!("Workspace {i}"),
                    auth_mode: "integration".into(),
                    ..Default::default()
                })
                .collect(),
            active_workspace: "ws-0".into(),
            ..Default::default()
        };

        let auth = AuthManager::from_token(cfg.clone(), "ntn_test_token");
        let client = NotionClient::new(&cfg, auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
        let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);

        let ui = UiState::from_config(&cfg);
        let mut app = AppState::new(cfg, auth, client, storage, sync_ctrl, ui_tx);

        let pages: Vec<_> = (0..6)
            .map(|i| NotionPage {
                id: format!("page-{i}"),
                workspace_id: app.workspace_id.clone(),
                title: format!("Page {i}"),
                ..Default::default()
            })
            .collect();

        app.pages = pages.iter().map(Into::into).collect();

        let media = (0..6)
            .map(|i| NotionMediaRef {
                block_id: format!("blk-{i}"),
                url: format!("https://example.com/media-{i}.png"),
                display_name: format!("image-{i}.png"),
                mime_type: "image/png".into(),
                is_image: true,
            })
            .collect();
        app.active_page_detail = Some(NotionPageDetail {
            page: pages[0].clone(),
            blocks: Vec::new(),
            comments: Vec::new(),
            comments_error: None,
            markdown: String::new(),
            media,
        });

        (app, ui)
    }

    fn wheel(delta: i16) -> Action {
        Action::MouseScroll {
            delta,
            column: 20,
            row: 10,
        }
    }

    #[test]
    fn test_mouse_scroll_navigates_all_list_and_menu_overlays_without_wrapping() {
        let (app, ui) = test_app_and_ui();

        // 1. ContextMenu (PageCanvas has 11 items)
        let mut ctx = Overlay::context_menu(ContextMenu::build(
            ContextTarget::PageCanvas,
            (4, 4),
            &app,
            &ui,
        ));
        let ctx_len = match &ctx {
            Overlay::ContextMenu(m) => m.items.len(),
            _ => unreachable!(),
        };
        assert!(ctx_len >= 6);
        assert_eq!(ctx.handle(&wheel(3), &app, &ui), OverlayOutcome::Handled);
        assert!(matches!(&ctx, Overlay::ContextMenu(m) if m.selected == 3));
        assert_eq!(ctx.handle(&wheel(100), &app, &ui), OverlayOutcome::Handled);
        assert!(matches!(&ctx, Overlay::ContextMenu(m) if m.selected == ctx_len - 1));
        assert_eq!(ctx.handle(&wheel(3), &app, &ui), OverlayOutcome::Handled);
        assert!(matches!(&ctx, Overlay::ContextMenu(m) if m.selected == ctx_len - 1));
        assert_eq!(ctx.handle(&wheel(-3), &app, &ui), OverlayOutcome::Handled);
        assert!(matches!(&ctx, Overlay::ContextMenu(m) if m.selected == ctx_len - 4));
        assert_eq!(ctx.handle(&wheel(-100), &app, &ui), OverlayOutcome::Handled);
        assert!(matches!(&ctx, Overlay::ContextMenu(m) if m.selected == 0));
        assert_eq!(ctx.handle(&wheel(-3), &app, &ui), OverlayOutcome::Handled);
        assert!(matches!(&ctx, Overlay::ContextMenu(m) if m.selected == 0));

        // 2. CommandPalette (6 pages + 16 workspace commands = 22 items)
        let mut palette = CommandPalette::new();
        assert_eq!(palette.handle(&wheel(3), &app, &ui), OverlayOutcome::Handled);
        assert_eq!(palette.selected, 3);
        assert_eq!(palette.handle(&Action::PageDown, &app, &ui), OverlayOutcome::Handled);
        assert_eq!(palette.selected, 8);
        assert_eq!(palette.handle(&wheel(100), &app, &ui), OverlayOutcome::Handled);
        assert_eq!(palette.selected, 21);
        assert_eq!(palette.handle(&wheel(3), &app, &ui), OverlayOutcome::Handled);
        assert_eq!(palette.selected, 21);
        assert_eq!(palette.handle(&wheel(-3), &app, &ui), OverlayOutcome::Handled);
        assert_eq!(palette.selected, 18);
        assert_eq!(palette.handle(&wheel(-100), &app, &ui), OverlayOutcome::Handled);
        assert_eq!(palette.selected, 0);
        assert_eq!(palette.handle(&wheel(-3), &app, &ui), OverlayOutcome::Handled);
        assert_eq!(palette.selected, 0);

        // 3. SearchOverlay (6 pages)
        let mut search = SearchOverlay::default();
        assert_eq!(search.handle(&wheel(3), &app), OverlayOutcome::Handled);
        assert_eq!(search.selected, 3);
        assert_eq!(search.handle(&wheel(100), &app), OverlayOutcome::Handled);
        assert_eq!(search.selected, 5);
        assert_eq!(search.handle(&wheel(3), &app), OverlayOutcome::Handled);
        assert_eq!(search.selected, 5);
        assert_eq!(search.handle(&wheel(-3), &app), OverlayOutcome::Handled);
        assert_eq!(search.selected, 2);
        assert_eq!(search.handle(&wheel(-100), &app), OverlayOutcome::Handled);
        assert_eq!(search.selected, 0);
        assert_eq!(search.handle(&wheel(-3), &app), OverlayOutcome::Handled);
        assert_eq!(search.selected, 0);

        // 4. FilePickerOverlay (both Browse and ZoxideJump modes)
        let tmp = tempfile::tempdir().unwrap();
        let mut zoxide_dirs = Vec::new();
        for i in 0..6 {
            let sub = tmp.path().join(format!("dir-{i}"));
            std::fs::create_dir(&sub).unwrap();
            zoxide_dirs.push(sub);
        }
        let mut picker = FilePickerOverlay::new_in_dir(tmp.path().to_path_buf());
        assert_eq!(picker.mode, FilePickerMode::Browse);
        assert_eq!(picker.handle(&wheel(3)), OverlayOutcome::Handled);
        assert_eq!(picker.selected, 3);
        assert_eq!(picker.handle(&wheel(100)), OverlayOutcome::Handled);
        assert_eq!(picker.selected, 5);
        assert_eq!(picker.handle(&wheel(3)), OverlayOutcome::Handled);
        assert_eq!(picker.selected, 5);
        assert_eq!(picker.handle(&wheel(-3)), OverlayOutcome::Handled);
        assert_eq!(picker.selected, 2);
        assert_eq!(picker.handle(&wheel(-100)), OverlayOutcome::Handled);
        assert_eq!(picker.selected, 0);
        assert_eq!(picker.handle(&wheel(-3)), OverlayOutcome::Handled);
        assert_eq!(picker.selected, 0);

        picker.enter_zoxide_with_dirs(zoxide_dirs);
        assert_eq!(picker.mode, FilePickerMode::ZoxideJump);
        assert_eq!(picker.handle(&wheel(3)), OverlayOutcome::Handled);
        assert_eq!(picker.zoxide_selected, 3);
        assert_eq!(picker.handle(&wheel(100)), OverlayOutcome::Handled);
        assert_eq!(picker.zoxide_selected, 5);
        assert_eq!(picker.handle(&wheel(3)), OverlayOutcome::Handled);
        assert_eq!(picker.zoxide_selected, 5);
        assert_eq!(picker.handle(&wheel(-3)), OverlayOutcome::Handled);
        assert_eq!(picker.zoxide_selected, 2);
        assert_eq!(picker.handle(&wheel(-100)), OverlayOutcome::Handled);
        assert_eq!(picker.zoxide_selected, 0);
        assert_eq!(picker.handle(&wheel(-3)), OverlayOutcome::Handled);
        assert_eq!(picker.zoxide_selected, 0);

        // 5. LinkExtractorOverlay (6 links)
        let mut links = LinkExtractorOverlay {
            context_title: "Test Page".into(),
            links: (0..6)
                .map(|i| ExtractedLink {
                    label: format!("Link {i}"),
                    url: format!("https://example.com/{i}"),
                })
                .collect(),
            selected: 0,
        };
        assert_eq!(links.handle(&wheel(3)), OverlayOutcome::Handled);
        assert_eq!(links.selected, 3);
        assert_eq!(links.handle(&wheel(100)), OverlayOutcome::Handled);
        assert_eq!(links.selected, 5);
        assert_eq!(links.handle(&wheel(3)), OverlayOutcome::Handled);
        assert_eq!(links.selected, 5);
        assert_eq!(links.handle(&wheel(-3)), OverlayOutcome::Handled);
        assert_eq!(links.selected, 2);
        assert_eq!(links.handle(&wheel(-100)), OverlayOutcome::Handled);
        assert_eq!(links.selected, 0);
        assert_eq!(links.handle(&wheel(-3)), OverlayOutcome::Handled);
        assert_eq!(links.selected, 0);

        // 6. AttachmentsState (6 media items)
        let mut attachments = AttachmentsState::default();
        assert_eq!(attachments.handle(&wheel(3), &app), OverlayOutcome::Handled);
        assert_eq!(attachments.selected, 3);
        assert_eq!(attachments.handle(&wheel(100), &app), OverlayOutcome::Handled);
        assert_eq!(attachments.selected, 5);
        assert_eq!(attachments.handle(&wheel(3), &app), OverlayOutcome::Handled);
        assert_eq!(attachments.selected, 5);
        assert_eq!(attachments.handle(&wheel(-3), &app), OverlayOutcome::Handled);
        assert_eq!(attachments.selected, 2);
        assert_eq!(attachments.handle(&wheel(-100), &app), OverlayOutcome::Handled);
        assert_eq!(attachments.selected, 0);
        assert_eq!(attachments.handle(&wheel(-3), &app), OverlayOutcome::Handled);
        assert_eq!(attachments.selected, 0);

        // 7. SettingsState (7 settings)
        let mut settings = SettingsState::default();
        assert_eq!(settings.handle(&wheel(3), &app), OverlayOutcome::Handled);
        assert_eq!(settings.selected, 3);
        assert_eq!(settings.handle(&wheel(100), &app), OverlayOutcome::Handled);
        assert_eq!(settings.selected, 6);
        assert_eq!(settings.handle(&wheel(3), &app), OverlayOutcome::Handled);
        assert_eq!(settings.selected, 6);
        assert_eq!(settings.handle(&wheel(-3), &app), OverlayOutcome::Handled);
        assert_eq!(settings.selected, 3);
        assert_eq!(settings.handle(&wheel(-100), &app), OverlayOutcome::Handled);
        assert_eq!(settings.selected, 0);
        assert_eq!(settings.handle(&wheel(-3), &app), OverlayOutcome::Handled);
        assert_eq!(settings.selected, 0);

        // 8. WorkspaceSwitcherOverlay (5 workspaces)
        let mut ws = WorkspaceSwitcherOverlay::default();
        assert_eq!(ws.handle(&wheel(3), &app), OverlayOutcome::Handled);
        assert_eq!(ws.selected, 3);
        assert_eq!(ws.handle(&wheel(100), &app), OverlayOutcome::Handled);
        assert_eq!(ws.selected, 4);
        assert_eq!(ws.handle(&wheel(3), &app), OverlayOutcome::Handled);
        assert_eq!(ws.selected, 4);
        assert_eq!(ws.handle(&wheel(-3), &app), OverlayOutcome::Handled);
        assert_eq!(ws.selected, 1);
        assert_eq!(ws.handle(&wheel(-100), &app), OverlayOutcome::Handled);
        assert_eq!(ws.selected, 0);
        assert_eq!(ws.handle(&wheel(-3), &app), OverlayOutcome::Handled);
        assert_eq!(ws.selected, 0);
    }

    #[test]
    fn test_help_links_and_attachments_shortcut_keys_end_to_end() {
        let (app, ui) = test_app_and_ui();

        // '?' closes HelpOverlay through key_to_action -> Overlay::handle
        let mut help = Overlay::help();
        let help_action = key_to_action(
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
            Focus::Overlay,
            &ui.keymap,
            help.accepts_text(),
            false,
            DatabaseViewMode::Table,
        )
        .expect("'?' should produce an action in Help overlay");
        assert_eq!(help_action, Action::Input('?'));
        assert_eq!(help.handle(&help_action, &app, &ui), OverlayOutcome::Close);

        // 'y' copies selected URL in LinkExtractorOverlay through key_to_action -> Overlay::handle
        let mut links = Overlay::links(LinkExtractorOverlay {
            context_title: "Roadmap".into(),
            links: vec![ExtractedLink {
                label: "Docs".into(),
                url: "https://notion.so/docs".into(),
            }],
            selected: 0,
        });
        let copy_action = key_to_action(
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
            Focus::Overlay,
            &ui.keymap,
            links.accepts_text(),
            false,
            DatabaseViewMode::Table,
        )
        .expect("'y' should produce an action in Links overlay");
        assert_eq!(copy_action, Action::Input('y'));
        assert_eq!(
            links.handle(&copy_action, &app, &ui),
            OverlayOutcome::Dispatch(Action::CopyUrlToClipboard(
                "https://notion.so/docs".into()
            ))
        );

        // 'v' opens ImagePreview from Attachments overlay through key_to_action -> Overlay::handle
        let mut attachments = Overlay::attachments();
        let preview_action = key_to_action(
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE),
            Focus::Overlay,
            &ui.keymap,
            attachments.accepts_text(),
            false,
            DatabaseViewMode::Table,
        )
        .expect("'v' should produce an action in Attachments overlay");
        assert_eq!(preview_action, Action::Input('v'));
        assert_eq!(
            attachments.handle(&preview_action, &app, &ui),
            OverlayOutcome::CloseAndDispatch(Action::OpenImagePreview)
        );
    }
}
