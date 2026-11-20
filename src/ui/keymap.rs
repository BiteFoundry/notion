//! Keyboard shortcut resolver for the Notion Terminal UI (`KeyMap`).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCommand {
    Quit,
    CommandPalette,
    WorkspaceSwitcher,
    Settings,
    Help,
    Search,
    Refresh,
    ToggleSidebar,
    ToggleWidth,
    ToggleFavorite,
    ToggleComments,
    QuickAppend,
    EditExternal,
    NewPage,
    ArchivePage,
    OpenFilePicker,
    OpenContextMenu,
    OpenLinkExtractor,
    ToggleShowLinkUrls,
    OpenImagePreview,
    OpenAttachments,
    OpenWeb,
    CopySelection,
    PasteClipboard,
    NavigateBack,
    SmoothScrollDown,
    SmoothScrollUp,
}

#[derive(Debug, Clone, Default)]
pub struct KeyMap;

impl KeyMap {
    /// Resolves the narrow subset of global shortcuts permitted while a modal overlay is active:
    /// `Ctrl+Q` (`Quit`) always, plus `Ctrl+D` / `Ctrl+U` (`SmoothScrollDown` / `SmoothScrollUp`)
    /// when the active overlay is not accepting text input.
    pub fn resolve_overlay_global(&self, key: &KeyEvent, text_input: bool) -> Option<KeyCommand> {
        if !key.modifiers.contains(KeyModifiers::CONTROL) {
            return None;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => Some(KeyCommand::Quit),
            KeyCode::Char('d') | KeyCode::Char('D') if !text_input => {
                Some(KeyCommand::SmoothScrollDown)
            }
            KeyCode::Char('u') | KeyCode::Char('U') if !text_input => {
                Some(KeyCommand::SmoothScrollUp)
            }
            _ => None,
        }
    }

    pub fn resolve_global(&self, key: &KeyEvent, text_input: bool) -> Option<KeyCommand> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        if ctrl {
            return match key.code {
                KeyCode::Char('q') | KeyCode::Char('Q') => Some(KeyCommand::Quit),
                KeyCode::Char('k') | KeyCode::Char('K') | KeyCode::Char('p') | KeyCode::Char('P') => {
                    Some(KeyCommand::CommandPalette)
                }
                KeyCode::Char('s') | KeyCode::Char('S') if !text_input => {
                    Some(KeyCommand::WorkspaceSwitcher)
                }
                KeyCode::Char(',') => Some(KeyCommand::Settings),
                KeyCode::Char('b') | KeyCode::Char('B') => Some(KeyCommand::ToggleSidebar),
                KeyCode::Char('r') | KeyCode::Char('R') => Some(KeyCommand::Refresh),
                KeyCode::Char('f') | KeyCode::Char('F') => Some(KeyCommand::OpenFilePicker),
                KeyCode::Char('v') | KeyCode::Char('V') => Some(KeyCommand::PasteClipboard),
                KeyCode::Char('d') | KeyCode::Char('D') if !text_input => {
                    Some(KeyCommand::SmoothScrollDown)
                }
                KeyCode::Char('u') | KeyCode::Char('U') if !text_input => {
                    Some(KeyCommand::SmoothScrollUp)
                }
                _ => None,
            };
        }

        if alt {
            return match key.code {
                KeyCode::Char('v') | KeyCode::Char('V') => Some(KeyCommand::OpenImagePreview),
                KeyCode::Left => Some(KeyCommand::NavigateBack),
                _ => None,
            };
        }

        if key.code == KeyCode::F(5) {
            return Some(KeyCommand::Refresh);
        }

        if text_input {
            return None;
        }

        match key.code {
            KeyCode::Char('?') => Some(KeyCommand::Help),
            KeyCode::Char('/') => Some(KeyCommand::Search),
            KeyCode::Char('.') => Some(KeyCommand::OpenContextMenu),
            KeyCode::Char('[') => Some(KeyCommand::ToggleSidebar),
            KeyCode::Char('w') => Some(KeyCommand::ToggleWidth),
            KeyCode::Char('C') => Some(KeyCommand::ToggleComments),
            KeyCode::Char('a') => Some(KeyCommand::QuickAppend),
            KeyCode::Char('e') => Some(KeyCommand::EditExternal),
            KeyCode::Char('n') => Some(KeyCommand::NewPage),
            KeyCode::Char('D') | KeyCode::Delete => Some(KeyCommand::ArchivePage),
            KeyCode::Char('u') => Some(KeyCommand::OpenLinkExtractor),
            KeyCode::Char('v') => Some(KeyCommand::OpenImagePreview),
            KeyCode::Char('A') => Some(KeyCommand::OpenAttachments),
            KeyCode::Char('o') => Some(KeyCommand::OpenWeb),
            KeyCode::Char('y') => Some(KeyCommand::CopySelection),
            KeyCode::Backspace => Some(KeyCommand::NavigateBack),
            _ => None,
        }
    }
}
