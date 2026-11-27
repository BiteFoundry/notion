//! Inline page editor, block-live editor state, and block text conversion helpers.

use ratatui::layout::Rect;
use unicode_width::UnicodeWidthChar;

use crate::{
    models::{spans_plain_text, NotionBlock, NotionBlockKind, NotionPageDetail, RichTextSpan},
    parser::{notion::rich_text_to_markdown, parse_inline_markdown_rich_text, parse_rich_text_array},
    ui::composer::{normalize, text_layout, MultiLineComposer},
};

use super::VisibleBlock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InlineSyncStatus {
    Synced,
    Syncing,
    Edited,
    Error,
}

/// In-place inline Markdown editor rendered directly inside the Page Canvas pane.
#[derive(Debug, Clone)]
pub struct InlinePageEditor {
    pub page_id: String,
    pub page_title: String,
    pub composer: MultiLineComposer,
    pub scroll_y: u16,
    pub follow_cursor: bool,
    pub last_viewport_height: u16,
    pub last_editor_area: Option<Rect>,
    pub last_visual_lines: Vec<(usize, usize)>,
    pub edit_generation: u64,
    pub synced_generation: u64,
    pub sync_in_flight: bool,
    pub last_sync_error: Option<String>,
    pub last_synced_markdown: String,
    pub last_synced_detail: Option<NotionPageDetail>,
    pub confirm_discard: bool,
    pub remote_blocks: Vec<NotionBlock>,
    pub remote_conflict: Option<NotionPageDetail>,
    pub confirm_overwrite: bool,
}

impl InlinePageEditor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        page_id: impl Into<String>,
        page_title: impl Into<String>,
        markdown: &str,
        initial_scroll: u16,
        edit_generation: u64,
        synced_generation: u64,
        sync_in_flight: bool,
        remote_blocks: Vec<NotionBlock>,
        last_synced_detail: Option<NotionPageDetail>,
    ) -> Self {
        let initial_markdown = normalize(markdown);
        let mut composer = MultiLineComposer::new();
        composer.load_text_cursor_start(&initial_markdown);
        Self {
            page_id: page_id.into(),
            page_title: page_title.into(),
            composer,
            scroll_y: initial_scroll,
            follow_cursor: false,
            last_viewport_height: 20,
            last_editor_area: None,
            last_visual_lines: Vec::new(),
            edit_generation,
            synced_generation,
            sync_in_flight,
            last_sync_error: None,
            last_synced_markdown: initial_markdown,
            last_synced_detail,
            confirm_discard: false,
            remote_blocks,
            remote_conflict: None,
            confirm_overwrite: false,
        }
    }

    pub fn cursor_line_col(&self) -> (usize, usize) {
        self.composer.cursor_line_col()
    }

    pub fn is_dirty(&self) -> bool {
        self.composer.text() != self.last_synced_markdown
    }

    pub fn sync_status(&self) -> (&'static str, InlineSyncStatus) {
        if self.last_sync_error.is_some() {
            ("⚠ Save failed", InlineSyncStatus::Error)
        } else if self.sync_in_flight {
            ("● Saving...", InlineSyncStatus::Syncing)
        } else if self.is_dirty() {
            ("● Unsaved", InlineSyncStatus::Edited)
        } else {
            ("✓ Saved", InlineSyncStatus::Synced)
        }
    }

    /// Maps a mouse click `(col, row)` inside the rendered editor body to the closest UTF-8
    /// character boundary in `self.composer` and moves the cursor there.
    pub fn place_cursor_at_screen_pos(&mut self, col: u16, row: u16) -> bool {
        let Some(area) = self.last_editor_area else {
            return false;
        };
        if row < area.y || row >= area.bottom() || col < area.x || col >= area.right() {
            return false;
        }
        let rel_row = (row - area.y) as usize;
        let rel_col = (col - area.x) as usize;
        let visual_idx = (self.scroll_y as usize).saturating_add(rel_row);
        let text = self.composer.text();
        let byte_pos = if let Some(&(start, end)) = self.last_visual_lines.get(visual_idx) {
            let slice = text.get(start..end).unwrap_or("");
            let mut used = 0usize;
            let mut found = end;
            for (offset, ch) in slice.char_indices() {
                let w = if ch == '\t' {
                    4
                } else {
                    UnicodeWidthChar::width(ch).unwrap_or(0)
                };
                if rel_col < used + w.max(1) {
                    found = start + offset;
                    break;
                }
                used += w;
            }
            found
        } else {
            text.len()
        };
        self.composer.set_cursor(byte_pos);
        self.follow_cursor = true;
        true
    }
}

/// Initial cursor placement when entering Notion Native Block-Live editing on a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockCursorInit {
    Start,
    End,
    ScreenPos { col: u16, row: u16 },
}

/// Screen layout of a rendered block on the Notion Page Canvas (rebuilt each frame).
#[derive(Debug, Clone)]
pub struct CanvasBlockLayout {
    pub visible_idx: usize,
    pub block_id: String,
    pub first_screen_row: u16,
    pub last_screen_row: u16,
    /// First screen row of the editable text (equals `first_screen_row`, except Code blocks where header is above).
    pub text_first_screen_row: u16,
    /// Screen X where editable text begins on the first text line (after gutter, indent, marker/checkbox/triangle).
    pub first_text_x: u16,
    /// Screen X where editable text begins on wrapped continuation lines.
    pub cont_text_x: u16,
    /// Effective text wrap width in columns.
    pub text_width: u16,
    /// Whether `[0..first_text_x)` on `first_screen_row` contains a leading checkbox or toggle triangle.
    pub has_leading_toggle: bool,
}

/// Clickable inline rich-text hyperlink rendered inside a Page Canvas block.
#[derive(Debug, Clone)]
pub struct CanvasLinkHit {
    pub rect: Rect,
    pub url: String,
}

/// In-place Notion Native Block-Live editor: edits a single block directly inside the rendered
/// Page Canvas while all other blocks stay 100% rendered.
#[derive(Debug, Clone)]
pub struct InlineBlockEditor {
    pub page_id: String,
    pub block_id: String,
    pub block_type: String,
    pub kind: NotionBlockKind,
    pub depth: usize,
    pub numbered_index: usize,
    pub is_toggle_open: bool,
    pub composer: MultiLineComposer,
    pub last_text_x: u16,
    pub last_first_screen_row: u16,
    pub last_screen_rows: u16,
    pub last_wrap_width: u16,
    pub last_visual_lines: Vec<(usize, usize)>,
    pub last_cursor_visual_row: usize,
    pub dirty: bool,
}

impl InlineBlockEditor {
    pub fn from_visible(page_id: impl Into<String>, vblk: &VisibleBlock) -> Self {
        let initial_text = normalize(&block_editable_text(&vblk.block.kind));
        let mut composer = MultiLineComposer::new();
        composer.load_text(&initial_text);
        Self {
            page_id: page_id.into(),
            block_id: vblk.block.id.clone(),
            block_type: block_type_for_kind(&vblk.block.kind).to_string(),
            kind: vblk.block.kind.clone(),
            depth: vblk.depth,
            numbered_index: vblk.numbered_index,
            is_toggle_open: vblk.is_toggle_open,
            composer,
            last_text_x: 0,
            last_first_screen_row: 0,
            last_screen_rows: 1,
            last_wrap_width: 72,
            last_visual_lines: Vec::new(),
            last_cursor_visual_row: 0,
            dirty: false,
        }
    }

    pub fn is_code(&self) -> bool {
        matches!(self.kind, NotionBlockKind::Code { .. })
    }

    pub fn effective_wrap_width(&self) -> usize {
        self.last_wrap_width.max(1) as usize
    }

    pub fn visual_layout(&self) -> (Vec<(usize, usize)>, usize, usize) {
        text_layout(
            self.composer.text(),
            self.effective_wrap_width(),
            self.composer.cursor(),
        )
    }

    /// Maps a visual line `(start, end)` and display column `target_col` to the closest UTF-8 byte offset.
    pub fn byte_at_display_col(text: &str, start: usize, end: usize, target_col: usize) -> usize {
        let slice = text.get(start..end).unwrap_or("");
        let mut used = 0usize;
        for (offset, ch) in slice.char_indices() {
            let w = if ch == '\t' {
                4
            } else {
                UnicodeWidthChar::width(ch).unwrap_or(0)
            };
            if target_col < used + w.max(1) {
                return start + offset;
            }
            used += w;
        }
        end
    }

    /// Places cursor at `(col, row)` using the last rendered screen geometry of this active block.
    pub fn place_cursor_at_screen_pos(&mut self, col: u16, row: u16) -> bool {
        let text = self.composer.text();
        let wrap_w = self.effective_wrap_width();
        let visual = if self.last_visual_lines.is_empty() {
            text_layout(text, wrap_w, self.composer.cursor()).0
        } else {
            self.last_visual_lines.clone()
        };
        let byte_pos = if row < self.last_first_screen_row {
            0
        } else {
            let rel_row = (row - self.last_first_screen_row) as usize;
            let rel_col = col.saturating_sub(self.last_text_x) as usize;
            if let Some(&(start, end)) = visual.get(rel_row) {
                Self::byte_at_display_col(text, start, end, rel_col)
            } else {
                text.len()
            }
        };
        self.composer.set_cursor(byte_pos);
        true
    }

    /// Places cursor at `(col, row)` using a `CanvasBlockLayout` captured while the block was rendered.
    pub fn place_cursor_from_layout(&mut self, col: u16, row: u16, layout: &CanvasBlockLayout) {
        self.last_text_x = layout.first_text_x;
        self.last_first_screen_row = layout.text_first_screen_row;
        self.last_wrap_width = layout.text_width.max(1);
        let text = self.composer.text();
        let (visual, _, _) = text_layout(text, self.last_wrap_width as usize, 0);
        self.last_visual_lines = visual.clone();
        let byte_pos = if row < layout.text_first_screen_row {
            0
        } else {
            let rel_row = (row - layout.text_first_screen_row) as usize;
            let base_x = if rel_row == 0 {
                layout.first_text_x
            } else {
                layout.cont_text_x
            };
            let rel_col = col.saturating_sub(base_x) as usize;
            if let Some(&(start, end)) = visual.get(rel_row) {
                Self::byte_at_display_col(text, start, end, rel_col)
            } else {
                text.len()
            }
        };
        self.composer.set_cursor(byte_pos);
    }

    /// Moves cursor up one visual line inside the block; returns `false` if already on the first visual line.
    pub fn move_visual_up(&mut self) -> bool {
        let (visual, cursor_row, cursor_col) = self.visual_layout();
        if cursor_row == 0 {
            return false;
        }
        let (start, end) = visual[cursor_row - 1];
        let target = Self::byte_at_display_col(self.composer.text(), start, end, cursor_col);
        self.composer.set_cursor(target);
        true
    }

    /// Moves cursor down one visual line inside the block; returns `false` if already on the last visual line.
    pub fn move_visual_down(&mut self) -> bool {
        let (visual, cursor_row, cursor_col) = self.visual_layout();
        if cursor_row + 1 >= visual.len() {
            return false;
        }
        let (start, end) = visual[cursor_row + 1];
        let target = Self::byte_at_display_col(self.composer.text(), start, end, cursor_col);
        self.composer.set_cursor(target);
        true
    }
}

/// Returns `true` for all Notion block kinds that support live in-place inline text editing.
pub fn is_block_inline_editable(kind: &NotionBlockKind) -> bool {
    matches!(
        kind,
        NotionBlockKind::Paragraph { .. }
            | NotionBlockKind::Heading1 { .. }
            | NotionBlockKind::Heading2 { .. }
            | NotionBlockKind::Heading3 { .. }
            | NotionBlockKind::BulletedListItem { .. }
            | NotionBlockKind::NumberedListItem { .. }
            | NotionBlockKind::ToDo { .. }
            | NotionBlockKind::Toggle { .. }
            | NotionBlockKind::Callout { .. }
            | NotionBlockKind::Quote { .. }
            | NotionBlockKind::Code { .. }
    )
}

/// Returns `true` for block kinds where pressing `Enter` in Canvas directly opens inline block edit
/// (toggle blocks keep `Enter` expanding/collapsing while `i` or text click edits them).
pub fn is_block_enter_editable(kind: &NotionBlockKind) -> bool {
    match kind {
        NotionBlockKind::Heading1 { is_toggleable, .. }
        | NotionBlockKind::Heading2 { is_toggleable, .. }
        | NotionBlockKind::Heading3 { is_toggleable, .. } => !is_toggleable,
        NotionBlockKind::Paragraph { .. }
        | NotionBlockKind::BulletedListItem { .. }
        | NotionBlockKind::NumberedListItem { .. }
        | NotionBlockKind::ToDo { .. }
        | NotionBlockKind::Callout { .. }
        | NotionBlockKind::Quote { .. }
        | NotionBlockKind::Code { .. } => true,
        _ => false,
    }
}

pub fn block_type_for_kind(kind: &NotionBlockKind) -> &'static str {
    match kind {
        NotionBlockKind::Paragraph { .. } => "paragraph",
        NotionBlockKind::Heading1 { .. } => "heading_1",
        NotionBlockKind::Heading2 { .. } => "heading_2",
        NotionBlockKind::Heading3 { .. } => "heading_3",
        NotionBlockKind::BulletedListItem { .. } => "bulleted_list_item",
        NotionBlockKind::NumberedListItem { .. } => "numbered_list_item",
        NotionBlockKind::ToDo { .. } => "to_do",
        NotionBlockKind::Toggle { .. } => "toggle",
        NotionBlockKind::Callout { .. } => "callout",
        NotionBlockKind::Quote { .. } => "quote",
        NotionBlockKind::Code { .. } => "code",
        _ => "paragraph",
    }
}

pub fn block_editable_text(kind: &NotionBlockKind) -> String {
    match kind {
        NotionBlockKind::Code { rich_text, .. } => spans_plain_text(rich_text),
        NotionBlockKind::Paragraph { rich_text, .. }
        | NotionBlockKind::Heading1 { rich_text, .. }
        | NotionBlockKind::Heading2 { rich_text, .. }
        | NotionBlockKind::Heading3 { rich_text, .. }
        | NotionBlockKind::BulletedListItem { rich_text, .. }
        | NotionBlockKind::NumberedListItem { rich_text, .. }
        | NotionBlockKind::ToDo { rich_text, .. }
        | NotionBlockKind::Toggle { rich_text, .. }
        | NotionBlockKind::Callout { rich_text, .. }
        | NotionBlockKind::Quote { rich_text, .. } => rich_text_to_markdown(rich_text),
        _ => String::new(),
    }
}

pub fn text_to_rich_spans(text: &str, is_code: bool) -> Vec<RichTextSpan> {
    if text.is_empty() {
        return Vec::new();
    }
    if is_code {
        return vec![RichTextSpan::plain(text)];
    }
    let vals = parse_inline_markdown_rich_text(text);
    parse_rich_text_array(Some(&serde_json::Value::Array(vals)))
}

pub fn apply_text_to_block_kind(kind: &mut NotionBlockKind, text: &str) {
    let is_code = matches!(kind, NotionBlockKind::Code { .. });
    let spans = text_to_rich_spans(text, is_code);
    match kind {
        NotionBlockKind::Paragraph { rich_text, .. }
        | NotionBlockKind::Heading1 { rich_text, .. }
        | NotionBlockKind::Heading2 { rich_text, .. }
        | NotionBlockKind::Heading3 { rich_text, .. }
        | NotionBlockKind::BulletedListItem { rich_text, .. }
        | NotionBlockKind::NumberedListItem { rich_text, .. }
        | NotionBlockKind::ToDo { rich_text, .. }
        | NotionBlockKind::Toggle { rich_text, .. }
        | NotionBlockKind::Callout { rich_text, .. }
        | NotionBlockKind::Quote { rich_text, .. }
        | NotionBlockKind::Code { rich_text, .. } => {
            *rich_text = spans;
        }
        _ => {}
    }
}

