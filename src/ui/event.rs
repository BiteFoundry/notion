//! Terminal keyboard & mouse event -> `Action` translation.

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use crate::{
    models::DatabaseViewMode,
    ui::{
        action::Action,
        app::Focus,
        hitmap::{HitMap, HitTarget},
        keymap::{KeyCommand, KeyMap},
        overlays::OverlayKind,
    },
};

pub fn key_to_action(
    key: KeyEvent,
    focus: Focus,
    keymap: &KeyMap,
    text_input: bool,
    is_database_active: bool,
    db_view_mode: DatabaseViewMode,
) -> Option<Action> {
    if key.kind == KeyEventKind::Release {
        return None;
    }

    if let Some(cmd) = keymap.resolve_global(&key, text_input) {
        return Some(match cmd {
            KeyCommand::Quit => Action::QuitConfirm,
            KeyCommand::CommandPalette => Action::OpenOverlay(OverlayKind::CommandPalette),
            KeyCommand::WorkspaceSwitcher => Action::OpenOverlay(OverlayKind::WorkspaceSwitcher),
            KeyCommand::Settings => Action::OpenOverlay(OverlayKind::Settings),
            KeyCommand::Help => Action::OpenOverlay(OverlayKind::Help),
            KeyCommand::Search => Action::OpenSearch,
            KeyCommand::Refresh => Action::RefreshAll,
            KeyCommand::ToggleSidebar => Action::ToggleSidebar,
            KeyCommand::ToggleWidth => Action::TogglePageWidth,
            KeyCommand::ToggleFavorite => Action::ToggleFavorite,
            KeyCommand::ToggleComments => Action::ToggleCommentsDrawer,
            KeyCommand::QuickAppend => Action::OpenQuickAppend,
            KeyCommand::EditExternal => Action::RequestExternalEditor,
            KeyCommand::NewPage => Action::OpenNewPageModal,
            KeyCommand::ArchivePage => Action::ConfirmArchivePage,
            KeyCommand::OpenFilePicker => Action::OpenFilePicker,
            KeyCommand::OpenContextMenu => Action::OpenContextMenu { col: 14, row: 4 },
            KeyCommand::OpenLinkExtractor => Action::OpenLinkExtractor,
            KeyCommand::ToggleShowLinkUrls => Action::ToggleShowLinkUrls,
            KeyCommand::OpenImagePreview => Action::OpenImagePreview,
            KeyCommand::OpenAttachments => Action::OpenOverlay(OverlayKind::Attachments),
            KeyCommand::OpenWeb => Action::OpenActiveInBrowser,
            KeyCommand::CopySelection => Action::CopySelectionOrMarkdown,
            KeyCommand::PasteClipboard => Action::PasteClipboard,
            KeyCommand::NavigateBack => Action::NavigateBack,
            KeyCommand::SmoothScrollDown => Action::PageDown,
            KeyCommand::SmoothScrollUp => Action::PageUp,
        });
    }

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    if focus == Focus::Overlay {
        if ctrl && key.code == KeyCode::Enter
            || alt && key.code == KeyCode::Enter
            || ctrl && matches!(key.code, KeyCode::Char('s') | KeyCode::Char('S'))
        {
            return Some(Action::OverlaySubmit);
        }
        return match key.code {
            KeyCode::Esc => Some(Action::CloseOverlay),
            KeyCode::Enter if shift => Some(Action::Newline),
            KeyCode::Enter => Some(Action::Activate),
            KeyCode::Tab => Some(Action::Tab),
            KeyCode::BackTab => Some(Action::PrevFocus),
            KeyCode::Backspace => Some(Action::Backspace),
            KeyCode::Delete => Some(Action::Delete),
            KeyCode::Up => Some(Action::MoveUp),
            KeyCode::Down => Some(Action::MoveDown),
            KeyCode::Left => Some(Action::CursorLeft),
            KeyCode::Right => Some(Action::CursorRight),
            KeyCode::Home => Some(Action::CursorHome),
            KeyCode::End => Some(Action::CursorEnd),
            KeyCode::PageUp => Some(Action::PageUp),
            KeyCode::PageDown => Some(Action::PageDown),
            KeyCode::Char(ch) if !ctrl && !alt => Some(Action::Input(ch)),
            _ => None,
        };
    }

    // Inline comment composer inside Comments Drawer
    if focus == Focus::Comments {
        return match key.code {
            KeyCode::Esc => Some(Action::ToggleCommentsDrawer),
            KeyCode::Tab => Some(Action::NextFocus),
            KeyCode::BackTab => Some(Action::PrevFocus),
            KeyCode::Enter => Some(Action::SubmitCommentReply),
            KeyCode::Backspace => Some(Action::Backspace),
            KeyCode::Up => Some(Action::MoveUp),
            KeyCode::Down => Some(Action::MoveDown),
            KeyCode::Char(ch) if !ctrl && !alt => Some(Action::Input(ch)),
            _ => None,
        };
    }

    // Database-specific view keys when viewing a Database in the main Canvas
    if is_database_active && focus == Focus::Canvas {
        match key.code {
            KeyCode::Char('1') => return Some(Action::SetDbViewMode(DatabaseViewMode::Table)),
            KeyCode::Char('2') => return Some(Action::SetDbViewMode(DatabaseViewMode::Board)),
            KeyCode::Char('3') => return Some(Action::SetDbViewMode(DatabaseViewMode::List)),
            KeyCode::Char('f') => return Some(Action::OpenDbFilter),
            KeyCode::Char('s') => return Some(Action::CycleDbSort),
            KeyCode::Char('m') if db_view_mode == DatabaseViewMode::Board => {
                return Some(Action::BoardMoveCardNextColumn);
            }
            KeyCode::Char('h') | KeyCode::Left if db_view_mode == DatabaseViewMode::Board => {
                return Some(Action::BoardPrevColumn);
            }
            KeyCode::Char('l') | KeyCode::Right if db_view_mode == DatabaseViewMode::Board => {
                return Some(Action::BoardNextColumn);
            }
            _ => {}
        }
    }

    // Page-canvas-specific keys
    if !is_database_active && focus == Focus::Canvas {
        match key.code {
            KeyCode::Char(' ') | KeyCode::Char('x') => return Some(Action::ToggleActiveBlock),
            KeyCode::Char('h') => return Some(Action::ToggleShowLinkUrls),
            KeyCode::Char('f') => return Some(Action::ToggleFavorite),
            _ => {}
        }
    }

    match key.code {
        KeyCode::Esc => Some(Action::CloseOverlay),
        KeyCode::Tab => Some(Action::NextFocus),
        KeyCode::BackTab => Some(Action::PrevFocus),
        KeyCode::Enter => Some(Action::Activate),
        KeyCode::Up | KeyCode::Char('k') => Some(Action::MoveUp),
        KeyCode::Down | KeyCode::Char('j') => Some(Action::MoveDown),
        KeyCode::Left => Some(Action::SidebarCollapseOrPrevFocus),
        KeyCode::Right => Some(Action::SidebarExpandOrNextFocus),
        KeyCode::Char(' ') if focus == Focus::Sidebar => Some(Action::Activate),
        KeyCode::Char('f') => Some(Action::ToggleFavorite),
        KeyCode::PageUp => Some(Action::PageUp),
        KeyCode::PageDown => Some(Action::PageDown),
        KeyCode::Home | KeyCode::Char('g') => Some(Action::ScrollTop),
        KeyCode::End | KeyCode::Char('G') => Some(Action::ScrollBottom),
        KeyCode::Char('q') => Some(Action::QuitConfirm),
        _ => None,
    }
}

pub fn mouse_to_action(mouse: MouseEvent, hitmap: &HitMap) -> Option<Action> {
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let target = hitmap.hit(mouse.row, mouse.column)?;
            Some(Action::MouseClick {
                target,
                col: mouse.column,
                row: mouse.row,
            })
        }
        MouseEventKind::Down(MouseButton::Right) => {
            let target = hitmap.hit(mouse.row, mouse.column);
            if matches!(target, Some(HitTarget::OverlayBody | HitTarget::OverlayItem { .. })) {
                return None;
            }
            Some(Action::OpenContextMenu {
                col: mouse.column,
                row: mouse.row,
            })
        }
        _ => None,
    }
}
