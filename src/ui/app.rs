//! Notion TUI application state (`AppState`), UI presentation state (`UiState`),
//! sidebar tree builder, visible block flattener (with live toggle expansion),
//! and database Table / Board (Kanban) / List query helpers.

use std::{borrow::Cow, collections::{HashMap, HashSet}};

use bitefoundry_html::NativeHtmlServer;
use ratatui::layout::Rect;
use tokio::sync::mpsc;
use unicode_width::UnicodeWidthChar;

use crate::{
    auth::AuthManager,
    config::{AppConfig, DensityMode, PageWidthMode},
    img_preview::{
        RenderedTerminalImage, TerminalGraphicsCaps, TerminalImageProtocol, MAX_IMAGE_CACHE_BYTES,
    },
    models::{
        spans_plain_text, CoreEvent, DatabaseSortMode, DatabaseViewMode, NotionBlock,
        NotionBlockKind, NotionColor, NotionComment, NotionDatabase, NotionPage, NotionPageDetail,
        NotionPageSummary, NotionPropertyValue, NotionUser, RichTextSpan, POLL_RETRY_SUMMARY,
    },
    notion::NotionClient,
    parser::{
        apply_optimistic_markdown_to_detail, notion::rich_text_to_markdown,
        parse_inline_markdown_rich_text, parse_rich_text_array,
    },
    storage::Storage,
    sync::{FocusTarget, SyncCommand, SyncController},
    ui::{
        composer::{normalize, text_layout, MultiLineComposer},
        hitmap::HitMap,
        keymap::KeyMap,
        overlays::Overlay,
        theme::Theme,
        toast::{TaskBoard, ToastQueue},
    },
};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    #[default]
    Sidebar,
    Canvas,
    Comments,
    Overlay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NarrowPane {
    Sidebar,
    #[default]
    Canvas,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CanvasView {
    #[default]
    Page,
    Database,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavTarget {
    Page(String),
    Database(String),
}

#[derive(Debug, Clone)]
pub struct PendingPixelGraphic {
    pub cache_key: String,
    pub rect: Rect,
    pub protocol: TerminalImageProtocol,
    /// Optional pre-encoded escape sequence payload (used by `HtmlPreviewOverlay` inline graphics
    /// whose rendered images live in the overlay's own cache rather than `app.image_cache`).
    pub payload: Option<String>,
}

/// Deepest WORKSPACE tree level the sidebar lists. The visited set already breaks parent cycles;
/// this only bounds pathological nesting, so real page trees are never silently cut off.
pub const SIDEBAR_MAX_TREE_DEPTH: usize = 20;

#[derive(Debug, Clone)]
pub struct SidebarItem {
    pub id: String,
    pub label: String,
    pub icon: String,
    pub depth: usize,
    pub is_database: bool,
    pub is_section_header: bool,
    pub collapsible: bool,
    pub collapsed: bool,
    pub is_favorite: bool,
    pub archived: bool,
}

#[derive(Debug, Clone)]
pub struct VisibleBlock {
    pub block: NotionBlock,
    pub depth: usize,
    pub is_toggle_open: bool,
    pub numbered_index: usize,
}

#[derive(Debug, Clone)]
pub struct BoardColumn {
    pub property_name: String,
    pub property_type: String,
    pub label: String,
    pub color: NotionColor,
    pub cards: Vec<NotionPage>,
    pub is_synthetic: bool,
}

pub enum UiMsg {
    StartLocalBootstrap,
    DaemonAttached {
        pid: Option<u32>,
        sync_busy: bool,
    },
    DaemonDetached,
    PageRefreshed {
        detail: Box<NotionPageDetail>,
        seq: u64,
        task_label: &'static str,
    },
    TodoToggleFailed {
        page_id: String,
        block_id: String,
        revert_checked: bool,
        error: String,
    },
    PageEdited {
        page_id: String,
        generation: u64,
        synced_markdown: String,
        detail: Box<NotionPageDetail>,
    },
    PageEditFailed {
        page_id: String,
        generation: u64,
        error: String,
        edited_markdown: String,
    },
    AppendFailed {
        page_id: String,
        error: String,
    },
    PageCreated {
        page: NotionPage,
        preview_detail: Option<Box<NotionPageDetail>>,
    },
    WorkspaceTreeRefreshed {
        workspace_id: String,
        pages: Vec<NotionPageSummary>,
        databases: Vec<NotionDatabase>,
        task_label: &'static str,
        toast: String,
    },
    PageArchived(String),
    MediaUrlRefreshed {
        page_id: String,
        block_id: String,
        url: String,
    },
    CommentAdded {
        page_id: String,
        comment: NotionComment,
    },
    BoardCardMoved {
        database_id: String,
        updated: NotionPage,
        column_label: String,
    },
    BoardCardMoveFailed {
        database_id: String,
        prev_col: usize,
        prev_row: usize,
        error: String,
    },
    WorkspaceSwitched {
        name: String,
        workspace_id: String,
    },
    ImageRendered {
        cache_key: String,
        rendered: Box<RenderedTerminalImage>,
    },
    ImagePreviewFailed {
        seq: u64,
        error: String,
    },
    HtmlPreviewLoaded {
        seq: u64,
        title: String,
        html: String,
        mime: Option<String>,
    },
    HtmlServed {
        url: String,
    },
    TaskDone {
        label: String,
        toast: String,
    },
    TaskFailed {
        label: String,
        error: String,
    },
    BlockChildrenHydrated {
        page_id: String,
        block_id: String,
        children: Vec<NotionBlock>,
    },
    BlockChildrenHydrationFailed {
        block_id: String,
        error: String,
    },
}

pub struct UiState {
    pub focus: Focus,
    pub narrow_pane: NarrowPane,
    pub show_sidebar: bool,
    pub page_width: PageWidthMode,
    pub density: DensityMode,
    pub inline_images: bool,
    pub show_inline_hrefs: bool,
    pub theme: Theme,
    pub keymap: KeyMap,
    pub hitmap: HitMap,
    pub toasts: ToastQueue,
    pub tasks: TaskBoard,
    pub overlays: Vec<Overlay>,
    pub inline_editor: Option<InlinePageEditor>,
    pub block_editor: Option<InlineBlockEditor>,
    pub canvas_block_layouts: Vec<CanvasBlockLayout>,
    pub canvas_link_hits: Vec<CanvasLinkHit>,
    pub page_confirmed_blocks: HashMap<String, Vec<NotionBlock>>,
    pub optimistic_block_seq: u64,
    pub cursor_pos: Option<(u16, u16)>,
    pub page_edit_generations: HashMap<String, u64>,
    pub page_synced_generations: HashMap<String, u64>,
    pub page_sync_in_flight: HashSet<String>,
    pub pending_page_sync: HashMap<String, (u64, String, Vec<NotionBlock>)>,
    pub page_refresh_seq: HashMap<String, u64>,
    pub preview_request_seq: u64,
    pub sidebar_selected: usize,
    pub collapsed_nodes: HashSet<String>,
    pub expanded_toggles: HashSet<String>,
    /// Blocks whose unloaded children are being fetched on demand (toggle expanded past the
    /// recursive tree-fetch depth cap).
    pub hydrating_blocks: HashSet<String>,
    pub selected_block: usize,
    pub canvas_scroll: u16,
    pub canvas_follow_selected: bool,
    pub comments_drawer_open: bool,
    pub comments_scroll: u16,
    pub comment_reply_input: String,
    pub db_view_mode: DatabaseViewMode,
    pub db_sort: DatabaseSortMode,
    pub db_filter: String,
    pub db_selected_row: usize,
    pub db_board_col: usize,
    pub db_board_row: usize,
    pub spinner_tick: usize,
    pub poll_retrying: bool,
    pub editor_requested: bool,
    pub external_editor_seed: Option<String>,
    pub term_width: u16,
    pub term_height: u16,
    pub graphics_caps: TerminalGraphicsCaps,
    pub kitty_keyboard_supported: bool,
    pub pending_pixel_graphics: Vec<PendingPixelGraphic>,
    pub active_pixel_graphic: Option<(String, Rect, TerminalImageProtocol)>,
}

impl Default for UiState {
    fn default() -> Self {
        Self::new()
    }
}

impl UiState {
    pub fn new() -> Self {
        Self::from_config(&AppConfig::default())
    }

    pub fn from_config(cfg: &AppConfig) -> Self {
        Self {
            focus: Focus::Sidebar,
            narrow_pane: NarrowPane::Canvas,
            show_sidebar: cfg.show_sidebar,
            page_width: cfg.page_width,
            density: cfg.density,
            inline_images: cfg.inline_images,
            show_inline_hrefs: cfg.show_inline_hrefs,
            theme: Theme::detect(cfg.theme),
            keymap: KeyMap,
            hitmap: HitMap::new(),
            toasts: ToastQueue::new(),
            tasks: TaskBoard::default(),
            overlays: Vec::new(),
            inline_editor: None,
            block_editor: None,
            canvas_block_layouts: Vec::new(),
            canvas_link_hits: Vec::new(),
            page_confirmed_blocks: HashMap::new(),
            optimistic_block_seq: 0,
            cursor_pos: None,
            page_edit_generations: HashMap::new(),
            page_synced_generations: HashMap::new(),
            page_sync_in_flight: HashSet::new(),
            pending_page_sync: HashMap::new(),
            page_refresh_seq: HashMap::new(),
            preview_request_seq: 0,
            sidebar_selected: 0,
            collapsed_nodes: HashSet::new(),
            expanded_toggles: HashSet::new(),
            hydrating_blocks: HashSet::new(),
            selected_block: 0,
            canvas_scroll: 0,
            canvas_follow_selected: false,
            comments_drawer_open: false,
            comments_scroll: 0,
            comment_reply_input: String::new(),
            db_view_mode: DatabaseViewMode::Table,
            db_sort: DatabaseSortMode::LastEditedDesc,
            db_filter: String::new(),
            db_selected_row: 0,
            db_board_col: 0,
            db_board_row: 0,
            spinner_tick: 0,
            poll_retrying: false,
            editor_requested: false,
            external_editor_seed: None,
            term_width: 120,
            term_height: 36,
            graphics_caps: TerminalGraphicsCaps::default(),
            kitty_keyboard_supported: false,
            pending_pixel_graphics: Vec::new(),
            active_pixel_graphic: None,
        }
    }

    pub fn focus(&self) -> Focus {
        if !self.overlays.is_empty() {
            Focus::Overlay
        } else {
            self.focus
        }
    }

    pub fn accepts_text_input(&self) -> bool {
        if let Some(top) = self.overlays.last() {
            return top.accepts_text();
        }
        if (self.inline_editor.is_some() || self.block_editor.is_some())
            && self.focus == Focus::Canvas
        {
            return true;
        }
        self.focus == Focus::Comments
    }

    pub fn info(&mut self, msg: impl Into<String>) {
        self.toasts.info(msg);
    }

    pub fn success(&mut self, msg: impl Into<String>) {
        self.toasts.success(msg);
    }

    pub fn warning(&mut self, msg: impl Into<String>) {
        self.toasts.warning(msg);
    }

    pub fn error(&mut self, msg: impl Into<String>) {
        self.toasts.error(msg);
    }

    pub fn next_page_refresh_seq(&mut self, page_id: &str) -> u64 {
        let entry = self.page_refresh_seq.entry(page_id.to_string()).or_insert(0);
        *entry = entry.wrapping_add(1);
        *entry
    }

    pub fn bump_preview_request_seq(&mut self) -> u64 {
        self.preview_request_seq = self.preview_request_seq.wrapping_add(1);
        self.preview_request_seq
    }
}

pub struct AppState {
    pub config: AppConfig,
    pub auth: AuthManager,
    pub client: NotionClient,
    pub storage: Storage,
    pub sync_ctrl: SyncController,
    pub ui_tx: mpsc::UnboundedSender<UiMsg>,
    /// Tells the sync supervisor to rebind to another workspace's cache. Unset when the app
    /// runs without a supervisor.
    pub workspace_switch_tx: Option<mpsc::UnboundedSender<String>>,
    /// The workspace switched to in this session. Tree updates of any other workspace are
    /// late results of the previous one and are dropped.
    pub expected_workspace_id: Option<String>,
    pub workspace_id: String,
    pub workspace_name: String,
    pub current_user: Option<NotionUser>,
    pub pages: Vec<NotionPageSummary>,
    pub databases: Vec<NotionDatabase>,
    pub block_owner_pages: HashMap<String, String>,
    pub child_page_positions: HashMap<String, i64>,
    pub active_view: CanvasView,
    pub active_page_id: Option<String>,
    pub active_page_detail: Option<NotionPageDetail>,
    pub active_database_id: Option<String>,
    pub active_database_rows: Vec<NotionPage>,
    /// The last sync read only the first rows of the active database.
    pub active_database_rows_truncated: bool,
    /// Rows the latest read of the active database returned (its row limit when truncated).
    pub active_database_rows_fetched: usize,
    pub nav_stack: Vec<NavTarget>,
    pub syncing: bool,
    pub sync_summary: String,
    pub image_cache: HashMap<String, RenderedTerminalImage>,
    image_cache_bytes: usize,
    pub html_server: NativeHtmlServer,
}

impl AppState {
    pub fn new(
        config: AppConfig,
        auth: AuthManager,
        client: NotionClient,
        storage: Storage,
        sync_ctrl: SyncController,
        ui_tx: mpsc::UnboundedSender<UiMsg>,
    ) -> Self {
        let workspace_name = config
            .active_profile()
            .map(|p| p.display_name().to_string())
            .unwrap_or_else(|| config.effective_workspace_name(None));
        let workspace_id = config
            .active_profile()
            .map(|p| p.effective_workspace_id().to_string())
            .unwrap_or_else(|| config.effective_workspace_name(None));
        Self {
            config,
            auth,
            client,
            storage,
            sync_ctrl,
            ui_tx,
            workspace_switch_tx: None,
            expected_workspace_id: None,
            workspace_id,
            workspace_name,
            current_user: None,
            pages: Vec::new(),
            databases: Vec::new(),
            block_owner_pages: HashMap::new(),
            child_page_positions: HashMap::new(),
            active_view: CanvasView::Page,
            active_page_id: None,
            active_page_detail: None,
            active_database_id: None,
            active_database_rows: Vec::new(),
            active_database_rows_truncated: false,
            active_database_rows_fetched: 0,
            nav_stack: Vec::new(),
            syncing: false,
            sync_summary: "Ready".to_string(),
            image_cache: HashMap::new(),
            image_cache_bytes: 0,
            html_server: NativeHtmlServer::new(),
        }
    }

    pub fn clear_image_cache(&mut self) {
        self.image_cache.clear();
        self.image_cache_bytes = 0;
    }

    pub fn reload_page_parent_hints(&mut self) {
        if let Ok((owners, positions)) = self.storage.load_page_parent_hints(&self.workspace_id) {
            self.block_owner_pages = owners;
            self.child_page_positions = positions;
        }
    }

    pub fn load_cached_initial(&mut self) {
        if let Ok(pages) = self.storage.list_page_summaries(&self.workspace_id) {
            self.pages = pages;
        }
        if let Ok(dbs) = self.storage.list_databases(&self.workspace_id) {
            self.databases = dbs;
        }
        self.reload_page_parent_hints();
        if self.active_page_id.is_none() && self.active_database_id.is_none() {
            if let Some(first_page) = self.pages.iter().find(|p| !p.archived).cloned() {
                self.active_view = CanvasView::Page;
                self.active_page_id = Some(first_page.id.clone());
                self.active_page_detail = self
                    .storage
                    .load_page_detail(&self.workspace_id, &first_page.id)
                    .ok()
                    .flatten();
                self.sync_ctrl.set_focus(FocusTarget {
                    page_id: Some(first_page.id),
                    database_id: None,
                });
            } else if let Some(first_db) = self.databases.first().cloned() {
                self.active_view = CanvasView::Database;
                self.active_database_id = Some(first_db.id.clone());
                self.active_database_rows = self
                    .storage
                    .list_database_rows(&self.workspace_id, &first_db.id)
                    .unwrap_or_default();
                self.sync_ctrl.set_focus(FocusTarget {
                    page_id: None,
                    database_id: Some(first_db.id),
                });
            }
        }
    }

    pub fn active_database(&self) -> Option<&NotionDatabase> {
        let id = self.active_database_id.as_deref()?;
        self.databases.iter().find(|d| d.id == id)
    }

    /// Requests a direct refresh of the currently visible page or database.
    pub fn refresh_active_item(&self) {
        match self.active_view {
            CanvasView::Page => {
                if let Some(page_id) = self.active_page_id.clone() {
                    self.sync_ctrl.request(SyncCommand::RefreshPage { page_id });
                }
            }
            CanvasView::Database => {
                if let Some(database_id) = self.active_database_id.clone() {
                    self.sync_ctrl
                        .request(SyncCommand::RefreshDatabase { database_id });
                }
            }
        }
    }

    fn merge_page_summary(&mut self, page: &NotionPage) -> bool {
        if let Some(existing) = self.pages.iter_mut().find(|p| p.id == page.id) {
            let mut summary = NotionPageSummary::from(page);
            summary.is_favorite |= existing.is_favorite;
            let changed = *existing != summary;
            *existing = summary;
            changed
        } else {
            false
        }
    }

    pub fn open_page(&mut self, page_id: &str, ui: &mut UiState, push_history: bool) {
        let _ = self.try_open_page(page_id, ui, push_history);
    }

    pub fn try_open_page(&mut self, page_id: &str, ui: &mut UiState, push_history: bool) -> bool {
        if ui.block_editor.is_some() {
            self.commit_and_sync_block_editor(ui, true);
        }
        if ui.inline_editor.is_some() && !self.close_inline_editor_and_flush(ui) {
            ui.focus = Focus::Canvas;
            ui.warning("Resolve or discard inline edits (Esc x2) before switching pages");
            return false;
        }
        if push_history {
            self.push_current_to_nav_stack();
        }
        self.active_view = CanvasView::Page;
        self.active_page_id = Some(page_id.to_string());
        ui.selected_block = 0;
        ui.canvas_scroll = 0;
        ui.canvas_follow_selected = false;

        if let Ok(Some(cached)) = self.storage.load_page_detail(&self.workspace_id, page_id) {
            self.active_page_detail = Some(cached);
        } else if let Some(p) = self.pages.iter().find(|p| p.id == page_id) {
            self.active_page_detail = Some(NotionPageDetail {
                page: NotionPage {
                    id: p.id.clone(),
                    workspace_id: p.workspace_id.clone(),
                    parent_type: p.parent_type.clone(),
                    parent_id: p.parent_id.clone(),
                    title: p.title.clone(),
                    icon: p.icon.clone(),
                    url: p.url.clone(),
                    archived: p.archived,
                    is_favorite: p.is_favorite,
                    last_edited_time: p.last_edited_time.clone(),
                    ..Default::default()
                },
                ..Default::default()
            });
        } else {
            self.active_page_detail = None;
        }
        self.clamp_canvas_selection(ui);
        self.sync_ctrl.set_focus(FocusTarget {
            page_id: Some(page_id.to_string()),
            database_id: None,
        });
        true
    }

    pub fn open_database(&mut self, database_id: &str, ui: &mut UiState, push_history: bool) {
        let _ = self.try_open_database(database_id, ui, push_history);
    }

    pub fn try_open_database(
        &mut self,
        database_id: &str,
        ui: &mut UiState,
        push_history: bool,
    ) -> bool {
        if ui.block_editor.is_some() {
            self.commit_and_sync_block_editor(ui, true);
        }
        if ui.inline_editor.is_some() && !self.close_inline_editor_and_flush(ui) {
            ui.focus = Focus::Canvas;
            ui.warning("Resolve or discard inline edits (Esc x2) before switching databases");
            return false;
        }
        if push_history {
            self.push_current_to_nav_stack();
        }
        if self.active_database_id.as_deref() != Some(database_id) {
            ui.db_filter.clear();
        }
        self.active_view = CanvasView::Database;
        self.active_page_id = None;
        self.active_page_detail = None;
        ui.comments_drawer_open = false;
        if ui.focus == Focus::Comments {
            ui.focus = Focus::Canvas;
        }
        self.active_database_id = Some(database_id.to_string());
        ui.db_selected_row = 0;
        ui.db_board_col = 0;
        ui.db_board_row = 0;
        ui.canvas_scroll = 0;

        self.active_database_rows = self
            .storage
            .list_database_rows(&self.workspace_id, database_id)
            .unwrap_or_default();
        self.active_database_rows_truncated = false;
        self.active_database_rows_fetched = 0;
        self.clamp_db_selection(ui);
        self.sync_ctrl.set_focus(FocusTarget {
            page_id: None,
            database_id: Some(database_id.to_string()),
        });
        true
    }

    pub fn snap_sidebar_selection(&self, ui: &mut UiState) {
        let items = self.build_sidebar_items(ui);
        if items.is_empty() {
            ui.sidebar_selected = 0;
            return;
        }
        let clamped = ui.sidebar_selected.min(items.len() - 1);
        if !items[clamped].is_section_header {
            ui.sidebar_selected = clamped;
            return;
        }
        if let Some(next) = ((clamped + 1)..items.len()).find(|&i| !items[i].is_section_header) {
            ui.sidebar_selected = next;
        } else if let Some(prev) = (0..clamped).rev().find(|&i| !items[i].is_section_header) {
            ui.sidebar_selected = prev;
        } else {
            ui.sidebar_selected = 0;
        }
    }

    pub fn clamp_db_selection(&self, ui: &mut UiState) {
        let rows_len = self.filtered_sorted_db_rows(ui).len();
        ui.db_selected_row = ui.db_selected_row.min(rows_len.saturating_sub(1));
        let cols = self.build_board_columns(ui);
        ui.db_board_col = ui.db_board_col.min(cols.len().saturating_sub(1));
        let cards_len = cols
            .get(ui.db_board_col)
            .map(|c| c.cards.len())
            .unwrap_or(0);
        ui.db_board_row = ui.db_board_row.min(cards_len.saturating_sub(1));
    }

    pub fn clamp_canvas_selection(&self, ui: &mut UiState) {
        let count = self.visible_blocks(ui).len();
        ui.selected_block = ui.selected_block.min(count.saturating_sub(1));
    }

    /// Opens the in-place inline Markdown editor directly inside the Page Canvas pane.
    pub fn open_inline_editor(&mut self, ui: &mut UiState) -> bool {
        if ui.block_editor.is_some() {
            self.commit_and_sync_block_editor(ui, true);
        }
        if self.active_view != CanvasView::Page {
            return false;
        }
        let Some(detail) = self.active_page_detail.as_ref() else {
            return false;
        };
        let page_id = detail.page.id.clone();
        let page_title = detail.page.display_title().to_string();
        let current_gen = ui.page_edit_generations.get(&page_id).copied().unwrap_or(0);
        let synced_gen = ui
            .page_synced_generations
            .get(&page_id)
            .copied()
            .unwrap_or(current_gen);
        let in_flight = ui.page_sync_in_flight.contains(&page_id);
        let remote_blocks = if !detail.blocks.is_empty()
            && detail
                .blocks
                .iter()
                .all(|b| !b.id.trim().is_empty() && !b.id.starts_with("__optimistic_"))
        {
            detail.blocks.clone()
        } else {
            Vec::new()
        };

        let last_synced_detail = Some(detail.clone());
        let initial_markdown = crate::page_edit::editor_markdown_for_detail(detail);

        ui.inline_editor = Some(InlinePageEditor::new(
            page_id,
            page_title,
            &initial_markdown,
            ui.canvas_scroll,
            current_gen,
            synced_gen,
            in_flight,
            remote_blocks,
            last_synced_detail,
        ));
        ui.focus = Focus::Canvas;
        ui.narrow_pane = NarrowPane::Canvas;
        true
    }

    /// Positions the active `InlinePageEditor` cursor near `block_idx` (preferring an exact
    /// line match on `target_snippet` closest to the proportional block line) and enables
    /// `follow_cursor` so the viewport scrolls straight to that block.
    pub fn position_inline_editor_near_block(
        &self,
        ui: &mut UiState,
        block_idx: usize,
        visible_len: usize,
        target_snippet: &str,
    ) {
        let Some(ed) = ui.inline_editor.as_mut() else {
            return;
        };
        let text = ed.composer.text();
        let mut lines: Vec<(usize, usize)> = Vec::new();
        let mut start = 0usize;
        for (i, ch) in text.char_indices() {
            if ch == '\n' {
                lines.push((start, i));
                start = i + 1;
            }
        }
        lines.push((start, text.len()));
        if lines.is_empty() {
            return;
        }

        let approx_line = if visible_len > 0 {
            block_idx
                .saturating_mul(lines.len())
                .checked_div(visible_len)
                .unwrap_or(0)
                .min(lines.len().saturating_sub(1))
        } else {
            0
        };

        let needle = target_snippet
            .lines()
            .map(str::trim)
            .find(|s| !s.is_empty())
            .unwrap_or("");

        let target_line = if !needle.is_empty() {
            lines
                .iter()
                .enumerate()
                .filter(|(_, &(s, e))| text.get(s..e).is_some_and(|line| line.contains(needle)))
                .min_by_key(|(idx, _)| idx.abs_diff(approx_line))
                .map(|(idx, _)| idx)
                .unwrap_or(approx_line)
        } else {
            approx_line
        };

        let (_, line_end) = lines[target_line];
        ed.composer.set_cursor(line_end);
        ed.follow_cursor = true;
    }

    /// Tracks a buffer mutation while the composer owns the draft and its undo history.
    pub fn record_inline_mutation(ui: &mut UiState) {
        let Some(ed) = ui.inline_editor.as_mut() else {
            return;
        };
        ed.edit_generation = ed.edit_generation.saturating_add(1);
        ed.last_sync_error = None;
        ed.confirm_discard = false;
        ed.confirm_overwrite = false;
        ui.page_edit_generations.insert(ed.page_id.clone(), ed.edit_generation);
    }

    /// Builds the optimistic canvas detail for a save or external editor handoff.
    pub fn apply_optimistic_local(&mut self, page_id: &str, edited_markdown: &str) {
        if let Some(ref mut detail) = self.active_page_detail {
            if detail.page.id == page_id
                && detail.markdown != edited_markdown
                && !crate::parser::has_column_blocks(&detail.blocks)
                && !crate::page_edit::contains_column_marker(edited_markdown)
            {
                apply_optimistic_markdown_to_detail(detail, edited_markdown);
            }
        }
    }

    /// Immediately appends optimistic blocks to the active in-memory page detail so quick-append
    /// renders in 0ms while the API request runs. Never writes `__optimistic_*` IDs to SQLite.
    pub fn apply_optimistic_append_local(
        &mut self,
        page_id: &str,
        markdown_or_slash: &str,
        ui: &mut UiState,
    ) -> bool {
        let optimistic = build_optimistic_append_blocks(page_id, markdown_or_slash);
        if optimistic.is_empty() {
            return false;
        }
        if let Some(ref mut detail) = self.active_page_detail {
            if detail.page.id == page_id {
                detail.blocks.extend(optimistic);
                detail.page.plain_text = crate::parser::blocks_to_plain_text(&detail.blocks);
                detail.markdown = crate::parser::blocks_to_markdown(&detail.blocks);
                detail.media = crate::parser::collect_media_refs(&detail.blocks);
                self.clamp_canvas_selection(ui);
                return true;
            }
        }
        false
    }

    /// Materializes the draft on demand for actions that read the page's blocks.
    pub fn detail_for_block_action(&self, ui: &UiState) -> Option<Cow<'_, NotionPageDetail>> {
        if self.active_view != CanvasView::Page {
            return None;
        }
        let detail = self.active_page_detail.as_ref()?;
        if let Some(ed) = ui.inline_editor.as_ref().filter(|ed| ed.page_id == detail.page.id) {
            let markdown = ed.composer.text();
            if markdown != detail.markdown
                && !crate::parser::has_column_blocks(&detail.blocks)
                && !crate::page_edit::contains_column_marker(markdown)
            {
                let mut draft = detail.clone();
                apply_optimistic_markdown_to_detail(&mut draft, markdown);
                return Some(Cow::Owned(draft));
            }
        }
        Some(Cow::Borrowed(detail))
    }

    /// Reflects the draft in the canvas and dispatches a background diff-sync, coalescing
    /// explicit saves while a previous save is in flight.
    pub fn flush_inline_editor_sync(&mut self, ui: &mut UiState, queue_if_in_flight: bool) -> bool {
        let Some(ed) = ui.inline_editor.as_mut() else {
            return false;
        };
        ed.confirm_discard = false;
        let page_id = ed.page_id.clone();
        if !ed.is_dirty() {
            return false;
        }

        if ed.remote_conflict.is_some() && !ed.confirm_overwrite {
            ed.confirm_overwrite = true;
            ui.warning(
                "Remote page changed — press Ctrl+S again to save your draft, or Esc x2 to reload",
            );
            return false;
        }

        let snapshot = ed.composer.text().to_string();
        let baseline_blocks = if ed.remote_conflict.is_some() {
            ed.remote_conflict
                .as_ref()
                .map(|d| d.blocks.clone())
                .unwrap_or_default()
        } else {
            ed.remote_blocks.clone()
        };
        if !baseline_blocks.is_empty() {
            if let Err(err) = crate::page_edit::PageEdit::plan(&page_id, &baseline_blocks, &snapshot) {
                ed.last_sync_error = Some(err.to_string());
                ui.error(format!("Cannot save: {err}"));
                return false;
            }
        }

        let overwrite_after_conflict = ed.remote_conflict.take().is_some();
        ed.confirm_overwrite = false;

        let next_gen = ed
            .edit_generation
            .max(ed.synced_generation)
            .max(ui.page_edit_generations.get(&page_id).copied().unwrap_or(0))
            .saturating_add(1);
        ed.edit_generation = next_gen;
        ui.page_edit_generations.insert(page_id.clone(), next_gen);

        let in_flight = ed.sync_in_flight || ui.page_sync_in_flight.contains(&page_id);
        let remote_blocks = if overwrite_after_conflict {
            Vec::new()
        } else {
            ed.remote_blocks.clone()
        };

        // Update in-memory detail immediately on explicit save, but persist SQLite only after
        // server confirmation so failed saves never leave `__optimistic_*` IDs in SQLite.
        self.apply_optimistic_local(&page_id, &snapshot);

        if in_flight {
            if queue_if_in_flight {
                ui.pending_page_sync
                    .insert(page_id, (next_gen, snapshot, remote_blocks));
            }
            return false;
        }

        if let Some(ed) = ui.inline_editor.as_mut() {
            ed.sync_in_flight = true;
            ed.last_sync_error = None;
        }
        ui.page_sync_in_flight.insert(page_id.clone());
        ui.pending_page_sync.remove(&page_id);

        let known = (!remote_blocks.is_empty()).then_some(remote_blocks);
        self.spawn_page_diff_sync(ui, page_id, next_gen, snapshot, known);
        true
    }

    /// Handles `Esc` inside the inline editor without firing unwanted Notion API syncs:
    /// - Clean buffer: exits immediately to rendered Canvas at `ed.scroll_y`.
    /// - Dirty buffer (1st `Esc`): arms `confirm_discard` and warns user (`Ctrl+S Save · Esc again Discard`).
    /// - Dirty buffer (2nd `Esc`): discards unsaved edits, restores `active_page_detail` from
    ///   `last_synced_detail`, and exits cleanly with 0 API calls.
    pub fn handle_inline_editor_escape(&mut self, ui: &mut UiState) {
        let Some(ed) = ui.inline_editor.as_mut() else {
            return;
        };
        if !ed.is_dirty() {
            let scroll = ed.scroll_y;
            ui.inline_editor = None;
            ui.canvas_scroll = scroll;
            ui.canvas_follow_selected = false;
            ui.cursor_pos = None;
            return;
        }
        if !ed.confirm_discard {
            ed.confirm_discard = true;
            ui.warning("Unsaved changes — Ctrl+S Save · Esc again Discard");
            return;
        }
        self.discard_inline_editor(ui);
    }

    /// Discards unsaved inline edits, restores `active_page_detail` to `last_synced_detail`,
    /// and exits inline edit mode without calling Notion API.
    pub fn discard_inline_editor(&mut self, ui: &mut UiState) {
        let Some(ed) = ui.inline_editor.take() else {
            return;
        };
        if let Some(saved_detail) = ed.last_synced_detail {
            if self.active_page_id.as_deref() == Some(ed.page_id.as_str()) {
                self.active_page_detail = Some(saved_detail);
            }
        }
        ui.canvas_scroll = ed.scroll_y;
        ui.canvas_follow_selected = false;
        ui.cursor_pos = None;
        ui.info("Discarded unsaved changes");
    }

    /// Exits inline edit mode cleanly, saving any unsynced edits immediately while
    /// preserving the user's current Canvas scroll position. Returns `false` if the editor
    /// refused to close due to a save error or unresolved remote conflict.
    pub fn close_inline_editor_and_flush(&mut self, ui: &mut UiState) -> bool {
        if ui.inline_editor.is_none() {
            return true;
        }
        if ui
            .inline_editor
            .as_ref()
            .is_some_and(|ed| ed.last_sync_error.is_some())
        {
            return false;
        }
        let _ = self.flush_inline_editor_sync(ui, true);
        if ui
            .inline_editor
            .as_ref()
            .is_some_and(|ed| ed.last_sync_error.is_some() || ed.confirm_overwrite)
        {
            return false;
        }
        if let Some(ed) = ui.inline_editor.take() {
            ui.canvas_scroll = ed.scroll_y;
            let visible_len = self.visible_blocks(ui).len();
            if visible_len > 0 {
                let total_lines = ed.composer.text().lines().count().max(1);
                let cursor_line = ed.composer.cursor_line_col().0.saturating_sub(1);
                let target_blk = (cursor_line.saturating_mul(visible_len) / total_lines)
                    .min(visible_len.saturating_sub(1));
                ui.selected_block = target_blk;
                ui.canvas_follow_selected = true;
            } else {
                ui.canvas_follow_selected = false;
            }
        }
        ui.cursor_pos = None;
        true
    }

    // ------------------------------------------------------------------
    // Notion Native Block-Live Flow (Single-Block In-Place Editing)
    // ------------------------------------------------------------------

    /// Captures the server-confirmed block tree baseline for `page_id` before the first optimistic
    /// block-live mutation, if not already cached and if all block IDs are real Notion IDs.
    fn ensure_page_confirmed_baseline(&self, ui: &mut UiState, page_id: &str) {
        if ui.page_confirmed_blocks.contains_key(page_id) {
            return;
        }
        if let Some(detail) = self.active_page_detail.as_ref().filter(|d| d.page.id == page_id) {
            if !detail.blocks.is_empty() && all_blocks_have_server_ids(&detail.blocks) {
                ui.page_confirmed_blocks
                    .insert(page_id.to_string(), detail.blocks.clone());
            }
        }
    }

    /// Opens Notion Native Block-Live editing directly IN PLACE on visible block `visible_idx`,
    /// keeping all other blocks on the page 100% rendered.
    pub fn open_block_editor(
        &mut self,
        ui: &mut UiState,
        visible_idx: usize,
        cursor_init: BlockCursorInit,
    ) -> bool {
        if self.active_view != CanvasView::Page || ui.inline_editor.is_some() {
            return false;
        }
        let Some(page_id) = self.active_page_detail.as_ref().map(|d| d.page.id.clone()) else {
            return false;
        };
        let visible = self.visible_blocks(ui);
        let Some(vblk) = visible.get(visible_idx).cloned() else {
            return false;
        };
        if !is_block_inline_editable(&vblk.block.kind) {
            return false;
        }

        // If already editing this exact block, just reposition the cursor without losing undo state.
        if let Some(existing) = ui
            .block_editor
            .as_mut()
            .filter(|b| b.page_id == page_id && b.block_id == vblk.block.id)
        {
            ui.selected_block = visible_idx;
            ui.focus = Focus::Canvas;
            ui.narrow_pane = NarrowPane::Canvas;
            match cursor_init {
                BlockCursorInit::Start => existing.composer.set_cursor(0),
                BlockCursorInit::End => existing.composer.move_doc_end(false),
                BlockCursorInit::ScreenPos { col, row } => {
                    existing.place_cursor_at_screen_pos(col, row);
                }
            }
            return true;
        }

        // Commit any previously active block before switching to a different block.
        if ui.block_editor.is_some() {
            self.commit_and_sync_block_editor(ui, true);
        }

        self.ensure_page_confirmed_baseline(ui, &page_id);

        // Re-fetch visible block in case committing updated the tree.
        let visible = self.visible_blocks(ui);
        let target_idx = visible
            .iter()
            .position(|vb| vb.block.id == vblk.block.id)
            .unwrap_or_else(|| visible_idx.min(visible.len().saturating_sub(1)));
        let Some(target_vblk) = visible.get(target_idx) else {
            return false;
        };
        if !is_block_inline_editable(&target_vblk.block.kind) {
            return false;
        }

        let mut editor = InlineBlockEditor::from_visible(page_id, target_vblk);
        let layout = ui
            .canvas_block_layouts
            .iter()
            .find(|l| l.block_id == target_vblk.block.id || l.visible_idx == target_idx)
            .cloned();
        if let Some(ref lay) = layout {
            editor.last_text_x = lay.first_text_x;
            editor.last_first_screen_row = lay.text_first_screen_row;
            editor.last_wrap_width = lay.text_width.max(1);
        }
        match cursor_init {
            BlockCursorInit::Start => editor.composer.set_cursor(0),
            BlockCursorInit::End => editor.composer.move_doc_end(false),
            BlockCursorInit::ScreenPos { col, row } => {
                if let Some(ref lay) = layout {
                    editor.place_cursor_from_layout(col, row, lay);
                } else {
                    editor.composer.move_doc_end(false);
                }
            }
        }

        ui.selected_block = target_idx;
        ui.canvas_follow_selected = true;
        ui.block_editor = Some(editor);
        ui.focus = Focus::Canvas;
        ui.narrow_pane = NarrowPane::Canvas;
        true
    }

    /// Immediately reflects the active `InlineBlockEditor`'s text and kind into
    /// `self.active_page_detail` (0ms optimistic local state, never touching SQLite).
    pub fn sync_active_block_to_detail(&mut self, ui: &mut UiState) {
        let Some(bed) = ui.block_editor.as_mut() else {
            return;
        };
        let text = bed.composer.text().to_string();
        apply_text_to_block_kind(&mut bed.kind, &text);
        let page_id = bed.page_id.clone();
        let block_id = bed.block_id.clone();
        let new_type = bed.block_type.clone();
        let new_kind = bed.kind.clone();

        if let Some(ref mut detail) = self.active_page_detail {
            if detail.page.id == page_id {
                if update_block_in_tree(&mut detail.blocks, &block_id, &new_type, &new_kind) {
                    detail.page.plain_text = crate::parser::blocks_to_plain_text(&detail.blocks);
                    detail.markdown = crate::parser::blocks_to_markdown(&detail.blocks);
                }
            }
        }
    }

    /// While editing a `Paragraph` block, converts live Markdown prefixes typed at the start
    /// (`# `, `## `, `### `, `- `, `* `, `[] `, `[ ] `, `> `) into the corresponding Notion block
    /// kind in 0ms, stripping the prefix and preserving any text after the cursor.
    pub fn try_convert_block_markdown_prefix(&mut self, ui: &mut UiState) -> bool {
        let converted = {
            let Some(bed) = ui.block_editor.as_mut() else {
                return false;
            };
            let NotionBlockKind::Paragraph { color, .. } = bed.kind else {
                return false;
            };
            let text = bed.composer.text();
            let cursor = bed.composer.cursor().min(text.len());
            if !text.is_char_boundary(cursor) {
                return false;
            }
            let prefix = &text[..cursor];
            let next_spec: Option<(&'static str, NotionBlockKind)> = match prefix {
                "# " => Some((
                    "heading_1",
                    NotionBlockKind::Heading1 {
                        rich_text: Vec::new(),
                        is_toggleable: false,
                        color,
                    },
                )),
                "## " => Some((
                    "heading_2",
                    NotionBlockKind::Heading2 {
                        rich_text: Vec::new(),
                        is_toggleable: false,
                        color,
                    },
                )),
                "### " => Some((
                    "heading_3",
                    NotionBlockKind::Heading3 {
                        rich_text: Vec::new(),
                        is_toggleable: false,
                        color,
                    },
                )),
                "- " | "* " => Some((
                    "bulleted_list_item",
                    NotionBlockKind::BulletedListItem {
                        rich_text: Vec::new(),
                        color,
                    },
                )),
                "[] " | "[ ] " => Some((
                    "to_do",
                    NotionBlockKind::ToDo {
                        rich_text: Vec::new(),
                        checked: false,
                        color,
                    },
                )),
                "> " => Some((
                    "quote",
                    NotionBlockKind::Quote {
                        rich_text: Vec::new(),
                        color,
                    },
                )),
                _ => None,
            };
            if let Some((new_type, mut new_kind)) = next_spec {
                let remainder = text[cursor..].to_string();
                apply_text_to_block_kind(&mut new_kind, &remainder);
                bed.block_type = new_type.to_string();
                bed.kind = new_kind;
                bed.composer.load_text_cursor_start(&remainder);
                bed.dirty = true;
                true
            } else {
                false
            }
        };
        if converted {
            self.sync_active_block_to_detail(ui);
        }
        converted
    }

    /// Pressing Backspace at cursor 0 of a Heading/List/To-Do/Quote converts it cleanly back to Paragraph.
    pub fn try_revert_block_to_paragraph_on_backspace(&mut self, ui: &mut UiState) -> bool {
        let reverted = {
            let Some(bed) = ui.block_editor.as_mut() else {
                return false;
            };
            if bed.composer.cursor() != 0 || bed.composer.has_selection() {
                return false;
            }
            let color = match bed.kind {
                NotionBlockKind::Heading1 { color, .. }
                | NotionBlockKind::Heading2 { color, .. }
                | NotionBlockKind::Heading3 { color, .. }
                | NotionBlockKind::BulletedListItem { color, .. }
                | NotionBlockKind::NumberedListItem { color, .. }
                | NotionBlockKind::ToDo { color, .. }
                | NotionBlockKind::Quote { color, .. } => color,
                _ => return false,
            };
            let text = bed.composer.text().to_string();
            let mut kind = NotionBlockKind::Paragraph {
                rich_text: Vec::new(),
                color,
            };
            apply_text_to_block_kind(&mut kind, &text);
            bed.block_type = "paragraph".to_string();
            bed.kind = kind;
            bed.dirty = true;
            true
        };
        if reverted {
            self.sync_active_block_to_detail(ui);
        }
        reverted
    }

    /// Commits the current non-Code block on `Enter`, splitting text after cursor (or inserting an
    /// empty sibling block below when at end) in 0ms, triggers coalesced background Notion sync,
    /// and moves inline editing focus directly into the newly created sibling block.
    pub fn split_active_block_on_enter(&mut self, ui: &mut UiState) -> bool {
        let Some(bed) = ui.block_editor.as_ref() else {
            return false;
        };
        if bed.is_code() {
            return false;
        }
        let page_id = bed.page_id.clone();
        let current_block_id = bed.block_id.clone();
        let current_kind = bed.kind.clone();
        let current_type = bed.block_type.clone();
        let text = bed.composer.text().to_string();
        let split_pos = bed.composer.cursor().min(text.len());
        let split_pos = if text.is_char_boundary(split_pos) {
            split_pos
        } else {
            text.len()
        };
        let before = text[..split_pos].to_string();
        let after = text[split_pos..].to_string();

        self.ensure_page_confirmed_baseline(ui, &page_id);

        let mut updated_current_kind = current_kind.clone();
        apply_text_to_block_kind(&mut updated_current_kind, &before);

        // Determine sibling block kind (Notion-native: headings/quotes/callouts continue as paragraph;
        // lists and to-dos continue with the same list/to-do kind).
        let (next_type, mut next_kind) = match &current_kind {
            NotionBlockKind::BulletedListItem { color, .. } if !before.is_empty() || !after.is_empty() => (
                "bulleted_list_item".to_string(),
                NotionBlockKind::BulletedListItem {
                    rich_text: Vec::new(),
                    color: *color,
                },
            ),
            NotionBlockKind::NumberedListItem { color, .. } if !before.is_empty() || !after.is_empty() => (
                "numbered_list_item".to_string(),
                NotionBlockKind::NumberedListItem {
                    rich_text: Vec::new(),
                    color: *color,
                },
            ),
            NotionBlockKind::ToDo { color, .. } if !before.is_empty() || !after.is_empty() => (
                "to_do".to_string(),
                NotionBlockKind::ToDo {
                    rich_text: Vec::new(),
                    checked: false,
                    color: *color,
                },
            ),
            _ => (
                "paragraph".to_string(),
                NotionBlockKind::Paragraph {
                    rich_text: Vec::new(),
                    color: NotionColor::Default,
                },
            ),
        };
        apply_text_to_block_kind(&mut next_kind, &after);

        ui.optimistic_block_seq = ui.optimistic_block_seq.saturating_add(1);
        let new_block_id = format!("__optimistic_live_{}_{}", page_id, ui.optimistic_block_seq);
        let parent_block_id = self
            .active_page_detail
            .as_ref()
            .and_then(|d| find_parent_block_id_in_tree(&d.blocks, &current_block_id));

        let new_block = NotionBlock {
            id: new_block_id.clone(),
            page_id: page_id.clone(),
            parent_block_id,
            block_type: next_type.clone(),
            has_children: false,
            archived: false,
            kind: next_kind.clone(),
            children: Vec::new(),
        };

        if let Some(ref mut detail) = self.active_page_detail {
            if detail.page.id == page_id {
                update_block_in_tree(
                    &mut detail.blocks,
                    &current_block_id,
                    &current_type,
                    &updated_current_kind,
                );
                if !insert_sibling_after_in_tree(&mut detail.blocks, &current_block_id, new_block) {
                    return false;
                }
                detail.page.plain_text = crate::parser::blocks_to_plain_text(&detail.blocks);
                detail.markdown = crate::parser::blocks_to_markdown(&detail.blocks);
            }
        } else {
            return false;
        }

        // Sync the committed edit + split to Notion in background (safely coalesced).
        self.sync_block_live_page(ui, &page_id);

        // Move inline editing focus directly into the newly inserted sibling block at cursor 0.
        let visible = self.visible_blocks(ui);
        if let Some(new_vis_idx) = visible.iter().position(|vb| vb.block.id == new_block_id) {
            let mut next_ed = InlineBlockEditor::from_visible(page_id, &visible[new_vis_idx]);
            next_ed.composer.set_cursor(0);
            next_ed.dirty = !after.is_empty();
            if let Some(prev_ed) = ui.block_editor.as_ref() {
                next_ed.last_wrap_width = prev_ed.last_wrap_width;
                next_ed.last_text_x = prev_ed.last_text_x;
            }
            ui.block_editor = Some(next_ed);
            ui.selected_block = new_vis_idx;
            ui.canvas_follow_selected = true;
            true
        } else {
            false
        }
    }

    /// Seamlessly moves inline block editing to the previous (`direction < 0`, cursor at end)
    /// or next (`direction > 0`, cursor at start) editable block, flushing/optimistically saving
    /// the current block first.
    pub fn navigate_block_editor(&mut self, ui: &mut UiState, direction: isize) -> bool {
        let Some((page_id, current_id, was_dirty)) = ui
            .block_editor
            .as_ref()
            .map(|b| (b.page_id.clone(), b.block_id.clone(), b.dirty))
        else {
            return false;
        };

        self.sync_active_block_to_detail(ui);
        let visible = self.visible_blocks(ui);
        let Some(cur_idx) = visible.iter().position(|vb| vb.block.id == current_id) else {
            return false;
        };

        let target_idx = if direction < 0 {
            (0..cur_idx)
                .rev()
                .find(|&i| is_block_inline_editable(&visible[i].block.kind))
        } else {
            ((cur_idx + 1)..visible.len())
                .find(|&i| is_block_inline_editable(&visible[i].block.kind))
        };

        let Some(next_idx) = target_idx else {
            if let Some(bed) = ui.block_editor.as_mut() {
                if direction < 0 {
                    bed.composer.set_cursor(0);
                } else {
                    bed.composer.move_doc_end(false);
                }
            }
            return false;
        };

        if was_dirty {
            self.sync_block_live_page(ui, &page_id);
        }

        let target_vblk = visible[next_idx].clone();
        let mut next_ed = InlineBlockEditor::from_visible(page_id, &target_vblk);
        if direction < 0 {
            next_ed.composer.move_doc_end(false);
        } else {
            next_ed.composer.set_cursor(0);
        }
        if let Some(lay) = ui
            .canvas_block_layouts
            .iter()
            .find(|l| l.block_id == target_vblk.block.id)
        {
            next_ed.last_text_x = lay.first_text_x;
            next_ed.last_first_screen_row = lay.text_first_screen_row;
            next_ed.last_wrap_width = lay.text_width.max(1);
        }
        ui.block_editor = Some(next_ed);
        ui.selected_block = next_idx;
        ui.canvas_follow_selected = true;
        true
    }

    /// Commits the active block editor's changes into `active_page_detail`, dispatches background
    /// sync if dirty, and optionally closes `ui.block_editor`.
    pub fn commit_and_sync_block_editor(&mut self, ui: &mut UiState, close_after: bool) -> bool {
        let Some((page_id, dirty)) = ui
            .block_editor
            .as_ref()
            .map(|b| (b.page_id.clone(), b.dirty))
        else {
            return false;
        };
        self.sync_active_block_to_detail(ui);
        let synced = if dirty {
            self.sync_block_live_page(ui, &page_id)
        } else {
            false
        };
        if close_after {
            ui.block_editor = None;
            ui.cursor_pos = None;
            ui.canvas_follow_selected = true;
        } else if let Some(bed) = ui.block_editor.as_mut() {
            bed.dirty = false;
        }
        synced
    }

    /// Coalesced background diff-sync for Notion Native Block-Live edits. Safe to call even while
    /// a previous sync is in flight or when new blocks still have `__optimistic_live_*` IDs.
    pub fn sync_block_live_page(&mut self, ui: &mut UiState, page_id: &str) -> bool {
        let Some(detail) = self.active_page_detail.as_ref().filter(|d| d.page.id == page_id) else {
            return false;
        };
        let edited_markdown = crate::page_edit::editor_markdown_for_detail(detail);
        let confirmed = ui.page_confirmed_blocks.get(page_id).cloned();

        // Skip redundant sync if markdown matches confirmed baseline exactly.
        if let Some(ref base_blocks) = confirmed {
            let base_md = if crate::parser::has_column_blocks(base_blocks) {
                crate::parser::blocks_to_editor_markdown(base_blocks)
            } else {
                crate::parser::blocks_to_markdown(base_blocks)
            };
            if base_md.trim() == edited_markdown.trim() {
                return false;
            }
        }

        let next_gen = ui
            .page_edit_generations
            .get(page_id)
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
        ui.page_edit_generations.insert(page_id.to_string(), next_gen);

        let known_blocks = confirmed
            .filter(|blocks| !blocks.is_empty() && all_blocks_have_server_ids(blocks));

        if ui.page_sync_in_flight.contains(page_id) {
            ui.pending_page_sync.insert(
                page_id.to_string(),
                (next_gen, edited_markdown, known_blocks.unwrap_or_default()),
            );
            return true;
        }

        if tokio::runtime::Handle::try_current().is_err() {
            return false;
        }

        ui.page_sync_in_flight.insert(page_id.to_string());
        ui.pending_page_sync.remove(page_id);
        self.spawn_page_diff_sync(
            ui,
            page_id.to_string(),
            next_gen,
            edited_markdown,
            known_blocks,
        );
        true
    }

    /// Submits an edited Markdown document (e.g. from external `$EDITOR`) with 0ms optimistic
    /// update + SQLite cache + fast diff-based background sync.
    pub fn submit_page_edit_markdown(&mut self, ui: &mut UiState, edited_markdown: String) -> bool {
        let Some(page_id) = self.active_page_id.clone() else {
            ui.warning("Open a page before saving edits");
            return false;
        };
        let current_md = self
            .active_page_detail
            .as_ref()
            .map(crate::page_edit::editor_markdown_for_detail)
            .unwrap_or_default();
        if edited_markdown.trim() == current_md.trim() {
            ui.info("No changes to save");
            return false;
        }
        let known_blocks = self.active_page_detail.as_ref().and_then(|d| {
            let valid = !d.blocks.is_empty()
                && d.blocks
                    .iter()
                    .all(|b| !b.id.trim().is_empty() && !b.id.starts_with("__optimistic_"));
            valid.then(|| d.blocks.clone())
        });
        if let Some(ref blocks) = known_blocks {
            if let Err(err) = crate::page_edit::PageEdit::plan(&page_id, blocks, &edited_markdown) {
                self.preserve_failed_page_edit(ui, &page_id, &edited_markdown, &err.to_string());
                return false;
            }
        }

        let next_gen = ui
            .page_edit_generations
            .get(&page_id)
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
        ui.page_edit_generations.insert(page_id.clone(), next_gen);

        self.apply_optimistic_local(&page_id, &edited_markdown);

        if ui.page_sync_in_flight.contains(&page_id) {
            ui.pending_page_sync.insert(
                page_id,
                (next_gen, edited_markdown, known_blocks.unwrap_or_default()),
            );
            return true;
        }

        ui.page_sync_in_flight.insert(page_id.clone());
        ui.pending_page_sync.remove(&page_id);
        self.spawn_page_diff_sync(ui, page_id, next_gen, edited_markdown, known_blocks);
        true
    }

    fn preserve_failed_page_edit(
        &mut self,
        ui: &mut UiState,
        page_id: &str,
        edited_markdown: &str,
        error: &str,
    ) {
        let recovery_note = match crate::page_edit::save_edit_recovery(page_id, edited_markdown) {
            Ok(path) => format!(" (draft saved to {})", path.display()),
            Err(_) => String::new(),
        };
        if self.active_page_id.as_deref() == Some(page_id) && self.active_view == CanvasView::Page {
            if let Some(detail) = self.active_page_detail.as_ref() {
                let current_gen = ui.page_edit_generations.get(page_id).copied().unwrap_or(0);
                let synced_gen = ui
                    .page_synced_generations
                    .get(page_id)
                    .copied()
                    .unwrap_or(current_gen);
                let initial_md = crate::page_edit::editor_markdown_for_detail(detail);
                let mut editor = InlinePageEditor::new(
                    page_id,
                    detail.page.display_title(),
                    &initial_md,
                    ui.canvas_scroll,
                    current_gen.saturating_add(1),
                    synced_gen,
                    false,
                    detail.blocks.clone(),
                    Some(detail.clone()),
                );
                editor.composer.load_text_cursor_start(edited_markdown);
                editor.last_sync_error = Some(error.to_string());
                ui.inline_editor = Some(editor);
                ui.focus = Focus::Canvas;
                ui.narrow_pane = NarrowPane::Canvas;
            }
        }
        ui.error(format!("Cannot save: {error}{recovery_note}"));
    }

    fn spawn_page_diff_sync(
        &self,
        ui: &mut UiState,
        page_id: String,
        generation: u64,
        edited_markdown: String,
        known_existing: Option<Vec<NotionBlock>>,
    ) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            ui.page_sync_in_flight.remove(&page_id);
            if let Some(ed) = ui.inline_editor.as_mut().filter(|e| e.page_id == page_id) {
                ed.sync_in_flight = false;
            }
            return;
        };
        let client = self.client.clone();
        let storage = self.storage.clone();
        let ui_tx = self.ui_tx.clone();
        ui.tasks.begin("edit");
        handle.spawn(async move {
            match crate::cli::op_diff_sync_page_markdown(
                &client,
                &storage,
                &page_id,
                &edited_markdown,
                known_existing,
            )
            .await
            {
                Ok(detail) => {
                    let _ = ui_tx.send(UiMsg::PageEdited {
                        page_id,
                        generation,
                        synced_markdown: edited_markdown,
                        detail: Box::new(detail),
                    });
                }
                Err(err) => {
                    let _ = ui_tx.send(UiMsg::PageEditFailed {
                        page_id,
                        generation,
                        error: err.to_string(),
                        edited_markdown,
                    });
                }
            }
        });
    }

    fn push_current_to_nav_stack(&mut self) {
        let current = match self.active_view {
            CanvasView::Page => self.active_page_id.clone().map(NavTarget::Page),
            CanvasView::Database => self.active_database_id.clone().map(NavTarget::Database),
        };
        if let Some(t) = current {
            if self.nav_stack.last() != Some(&t) {
                self.nav_stack.push(t);
                if self.nav_stack.len() > 20 {
                    self.nav_stack.remove(0);
                }
            }
        }
    }

    pub fn toggle_todo_block(&mut self, block_id: &str, next_checked: bool, ui: &mut UiState) {
        // Optimistic local update for instant feedback
        if let Some(ref mut detail) = self.active_page_detail {
            set_todo_in_tree(&mut detail.blocks, block_id, next_checked);
            detail.page.plain_text = crate::parser::blocks_to_plain_text(&detail.blocks);
            detail.markdown = crate::parser::blocks_to_markdown(&detail.blocks);
        }
        let client = self.client.clone();
        let storage = self.storage.clone();
        let ui_tx = self.ui_tx.clone();
        let bid = block_id.to_string();
        let page_id = self.active_page_id.clone().unwrap_or_default();
        let ws_id = self.workspace_id.clone();
        let req_seq = ui.next_page_refresh_seq(&page_id);
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        ui.tasks.begin("todo");
        handle.spawn(async move {
            match client.set_todo_checked(&bid, &page_id, next_checked).await {
                Ok(updated_block) => {
                    let st = storage.clone();
                    let ws_apply = ws_id.clone();
                    let blk_apply = updated_block.clone();
                    let applied = tokio::task::spawn_blocking(move || {
                        let res = st.apply_updated_block(&ws_apply, &blk_apply)?;
                        let detail = match &res {
                            Some((pid, false)) => st.load_page_detail(&ws_apply, pid)?,
                            _ => None,
                        };
                        Ok::<_, anyhow::Error>((res, detail))
                    })
                    .await;

                    if let Ok(Ok((Some((_, false)), Some(detail)))) = applied {
                        let _ = ui_tx.send(UiMsg::PageRefreshed {
                            detail: Box::new(detail),
                            seq: req_seq,
                            task_label: "todo",
                        });
                        return;
                    }

                    // Server response alone could not restore the cached tree: invalidate only this
                    // page's detail marker and re-fetch directly while open.
                    let st_inv = storage.clone();
                    let ws_inv = ws_id.clone();
                    let pid_inv = page_id.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        st_inv.invalidate_page_detail(&ws_inv, &pid_inv)
                    })
                    .await;

                    if let Ok(detail) =
                        crate::sync::sync_page_detail_force(&client, &storage, &page_id).await
                    {
                        let _ = ui_tx.send(UiMsg::PageRefreshed {
                            detail: Box::new(detail),
                            seq: req_seq,
                            task_label: "todo",
                        });
                    } else {
                        let _ = ui_tx.send(UiMsg::TaskDone {
                            label: "todo".into(),
                            toast: "Updated to-do".into(),
                        });
                    }
                }
                Err(err) => {
                    let _ = ui_tx.send(UiMsg::TodoToggleFailed {
                        page_id,
                        block_id: bid,
                        revert_checked: !next_checked,
                        error: err.to_string(),
                    });
                }
            }
        });
    }

    pub fn handle_core_event(&mut self, ui: &mut UiState, ev: CoreEvent) -> bool {
        match ev {
            CoreEvent::ProfileLoaded(user) => {
                if let Some(ref ws_name) = user.workspace_name {
                    if !ws_name.trim().is_empty() {
                        self.workspace_name = ws_name.clone();
                    }
                }
                self.current_user = Some(user);
                true
            }
            CoreEvent::WorkspaceTreeUpdated {
                workspace_id,
                pages,
                databases,
            } => {
                if self
                    .expected_workspace_id
                    .as_ref()
                    .is_some_and(|expected| *expected != workspace_id)
                {
                    return false;
                }
                self.workspace_id = workspace_id;
                self.pages = pages;
                self.databases = databases;
                self.reload_page_parent_hints();
                if self.active_page_id.is_none() && self.active_database_id.is_none() {
                    if let Some(first) = self.pages.iter().find(|p| !p.archived).cloned() {
                        self.open_page(&first.id, ui, false);
                    } else if let Some(first_db) = self.databases.first().cloned() {
                        self.open_database(&first_db.id, ui, false);
                    }
                }
                self.snap_sidebar_selection(ui);
                true
            }
            CoreEvent::PageDetailUpdated(detail) => {
                if let Some(expected) = self.expected_workspace_id.as_deref() {
                    if !detail.page.workspace_id.is_empty() && detail.page.workspace_id != expected
                    {
                        return false;
                    }
                    if detail.page.workspace_id.is_empty()
                        && self.active_page_id.as_deref() != Some(detail.page.id.as_str())
                        && !self.pages.iter().any(|p| p.id == detail.page.id)
                    {
                        return false;
                    }
                }
                let syncing_own_edit = ui.page_sync_in_flight.contains(&detail.page.id)
                    || ui.pending_page_sync.contains_key(&detail.page.id);
                if syncing_own_edit {
                    return false;
                }
                if ui
                    .block_editor
                    .as_ref()
                    .is_some_and(|b| b.page_id == detail.page.id)
                {
                    if all_blocks_have_server_ids(&detail.blocks) {
                        ui.page_confirmed_blocks
                            .insert(detail.page.id.clone(), detail.blocks.clone());
                    }
                    return self.merge_page_summary(&detail.page);
                }
                let summary_changed = self.merge_page_summary(&detail.page);
                let active_editor_matches = ui
                    .inline_editor
                    .as_ref()
                    .is_some_and(|ed| ed.page_id == detail.page.id);
                if active_editor_matches {
                    let incoming_md =
                        normalize(&crate::page_edit::editor_markdown_for_detail(&detail));
                    let mut visual_changed = summary_changed;
                    if let Some(ed) = ui.inline_editor.as_mut() {
                        let next_title = detail.page.display_title().to_string();
                        if ed.page_title != next_title {
                            ed.page_title = next_title;
                            visual_changed = true;
                        }
                        let prev_ts = ed
                            .last_synced_detail
                            .as_ref()
                            .and_then(|d| d.page.last_edited_time.as_deref());
                        let remote_changed = incoming_md != ed.last_synced_markdown
                            || detail.page.last_edited_time.as_deref() != prev_ts;
                        if !ed.is_dirty() {
                            if remote_changed {
                                ed.composer.load_text_cursor_start(&incoming_md);
                                ed.last_synced_markdown = incoming_md;
                                visual_changed = true;
                            }
                            ed.remote_blocks = detail.blocks.clone();
                            ed.last_synced_detail = Some((*detail).clone());
                            ed.remote_conflict = None;
                            ed.confirm_overwrite = false;
                            if self.active_page_id.as_deref() == Some(detail.page.id.as_str()) {
                                let detail_changed = self.active_page_detail.as_ref().map_or(true, |cur| {
                                    !crate::sync::page_detail_semantically_equal(cur, &detail)
                                });
                                if detail_changed {
                                    self.reload_page_parent_hints();
                                    self.active_page_detail = Some(*detail);
                                    visual_changed = true;
                                }
                            }
                        } else if remote_changed {
                            let same_conflict = ed.remote_conflict.as_ref().is_some_and(|prev| {
                                normalize(&crate::page_edit::editor_markdown_for_detail(prev))
                                    == incoming_md
                                    && prev.page.last_edited_time == detail.page.last_edited_time
                                    && prev.comments == detail.comments
                            });
                            if !same_conflict {
                                // Preserve the user's active draft buffer; record conflict for Ctrl+S guard
                                // and update `last_synced_detail` so discarding restores latest remote.
                                ed.remote_conflict = Some((*detail).clone());
                                ed.last_synced_detail = Some(*detail);
                                ed.confirm_overwrite = false;
                                ui.warning(
                                    "Remote page changed — draft preserved (Ctrl+S twice to overwrite, Esc x2 to reload)",
                                );
                                visual_changed = true;
                            }
                        } else if self.active_page_id.as_deref() == Some(detail.page.id.as_str()) {
                            if let Some(active) = self.active_page_detail.as_mut() {
                                if active.page.title != detail.page.title
                                    || active.page.icon != detail.page.icon
                                    || active.comments != detail.comments
                                {
                                    active.page.title = detail.page.title;
                                    active.page.icon = detail.page.icon;
                                    active.comments = detail.comments;
                                    visual_changed = true;
                                }
                            }
                        }
                    }
                    return visual_changed;
                }
                if self.active_page_id.as_deref() == Some(detail.page.id.as_str()) {
                    let detail_changed = self.active_page_detail.as_ref().map_or(true, |cur| {
                        !crate::sync::page_detail_semantically_equal(cur, &detail)
                    });
                    if !detail_changed && !summary_changed {
                        return false;
                    }
                    self.reload_page_parent_hints();
                    self.active_page_detail = Some(*detail);
                    self.clamp_canvas_selection(ui);
                    return true;
                }
                if summary_changed {
                    self.reload_page_parent_hints();
                }
                summary_changed
            }
            CoreEvent::DatabaseRowsUpdated {
                database_id,
                database,
                rows,
                rows_truncated,
            } => {
                if let Some(expected) = self.expected_workspace_id.as_deref() {
                    let event_ws = database
                        .as_ref()
                        .map(|d| d.workspace_id.as_str())
                        .filter(|s| !s.is_empty())
                        .or_else(|| {
                            rows.first()
                                .map(|r| r.workspace_id.as_str())
                                .filter(|s| !s.is_empty())
                        });
                    if let Some(ws) = event_ws {
                        if ws != expected {
                            return false;
                        }
                    } else if self.active_database_id.as_deref() != Some(database_id.as_str())
                        && !self.databases.iter().any(|d| d.id == database_id)
                    {
                        return false;
                    }
                }
                let mut changed = false;
                if let Some(db) = database {
                    if let Some(existing) = self.databases.iter_mut().find(|d| d.id == db.id) {
                        if !crate::sync::database_semantically_equal(existing, &db) {
                            *existing = db;
                            changed = true;
                        }
                    } else {
                        self.databases.push(db);
                        changed = true;
                    }
                }
                if self.active_database_id.as_deref() == Some(database_id.as_str()) {
                    let fetched_rows = rows.len();
                    if rows_truncated && self.active_database_rows.len() > rows.len() {
                        // Preserve already-loaded rows beyond a truncated background poll slice
                        // while refreshing any rows present in the poll window.
                        let mut by_id: HashMap<String, usize> = self
                            .active_database_rows
                            .iter()
                            .enumerate()
                            .map(|(i, r)| (r.id.clone(), i))
                            .collect();
                        for incoming in rows {
                            if let Some(&idx) = by_id.get(&incoming.id) {
                                if !crate::sync::page_semantically_equal(
                                    &self.active_database_rows[idx],
                                    &incoming,
                                ) {
                                    self.active_database_rows[idx] = incoming;
                                    changed = true;
                                }
                            } else {
                                by_id.insert(incoming.id.clone(), self.active_database_rows.len());
                                self.active_database_rows.push(incoming);
                                changed = true;
                            }
                        }
                    } else {
                        let rows_same = self.active_database_rows.len() == rows.len()
                            && self
                                .active_database_rows
                                .iter()
                                .zip(rows.iter())
                                .all(|(a, b)| crate::sync::page_semantically_equal(a, b));
                        if !rows_same {
                            self.active_database_rows = rows;
                            changed = true;
                        }
                    }
                    if self.active_database_rows_truncated != rows_truncated {
                        self.active_database_rows_truncated = rows_truncated;
                        changed = true;
                    }
                    if self.active_database_rows_fetched != fetched_rows {
                        self.active_database_rows_fetched = fetched_rows;
                        changed |= rows_truncated;
                    }
                    self.clamp_db_selection(ui);
                }
                changed
            }
            CoreEvent::CommentsUpdated { page_id, comments } => {
                if self.active_page_id.as_deref() != Some(page_id.as_str()) {
                    return false;
                }
                if let Some(ref mut detail) = self.active_page_detail {
                    if let Some(expected) = self.expected_workspace_id.as_deref() {
                        if !detail.page.workspace_id.is_empty()
                            && detail.page.workspace_id != expected
                        {
                            return false;
                        }
                    }
                    if detail.page.id == page_id && detail.comments != comments {
                        detail.comments = comments;
                        return true;
                    }
                }
                false
            }
            CoreEvent::SyncStateChanged { busy, summary } => {
                let next_retry = summary == POLL_RETRY_SUMMARY;
                let changed = self.syncing != busy
                    || ui.poll_retrying != next_retry
                    || self.sync_summary != summary;
                self.syncing = busy;
                ui.poll_retrying = next_retry;
                self.sync_summary = summary;
                changed
            }
            CoreEvent::SyncError(err) => {
                ui.error(format!("Sync: {err}"));
                true
            }
        }
    }

    pub fn handle_ui_msg(&mut self, ui: &mut UiState, msg: UiMsg) {
        match msg {
            UiMsg::StartLocalBootstrap => {
                self.sync_ctrl.request(SyncCommand::RefreshAll);
            }
            UiMsg::DaemonAttached { pid, sync_busy } => {
                self.syncing = sync_busy;
                if let Some(p) = pid {
                    ui.info(format!("Attached to sync daemon (pid {p})"));
                }
            }
            UiMsg::DaemonDetached => {
                ui.warning("Sync daemon detached; switched to local sync");
            }
            UiMsg::PageRefreshed {
                detail,
                seq,
                task_label,
            } => {
                ui.tasks.finish(task_label, None);
                if let Some(expected) = self.expected_workspace_id.as_deref() {
                    if !detail.page.workspace_id.is_empty() && detail.page.workspace_id != expected
                    {
                        return;
                    }
                }
                let latest_seq = ui
                    .page_refresh_seq
                    .get(&detail.page.id)
                    .copied()
                    .unwrap_or(0);
                if seq < latest_seq {
                    return;
                }
                let syncing_own_edit = ui.page_sync_in_flight.contains(&detail.page.id)
                    || ui.pending_page_sync.contains_key(&detail.page.id);
                if syncing_own_edit {
                    return;
                }
                self.merge_page_summary(&detail.page);
                let editing_this_page = ui
                    .inline_editor
                    .as_ref()
                    .is_some_and(|ed| ed.page_id == detail.page.id)
                    || ui
                        .block_editor
                        .as_ref()
                        .is_some_and(|b| b.page_id == detail.page.id);
                if all_blocks_have_server_ids(&detail.blocks) {
                    ui.page_confirmed_blocks
                        .insert(detail.page.id.clone(), detail.blocks.clone());
                }
                if self.active_page_id.as_deref() == Some(detail.page.id.as_str())
                    && !editing_this_page
                {
                    self.active_page_detail = Some(*detail);
                    self.clamp_canvas_selection(ui);
                }
                self.reload_page_parent_hints();
                ui.success("Synced page");
            }
            UiMsg::TodoToggleFailed {
                page_id,
                block_id,
                revert_checked,
                error,
            } => {
                ui.tasks.finish("todo", Some(error.clone()));
                let editing_this_page = ui
                    .inline_editor
                    .as_ref()
                    .is_some_and(|ed| ed.page_id == page_id);
                if self.active_page_id.as_deref() == Some(page_id.as_str()) && !editing_this_page {
                    if let Some(ref mut detail) = self.active_page_detail {
                        set_todo_in_tree(&mut detail.blocks, &block_id, revert_checked);
                        detail.page.plain_text = crate::parser::blocks_to_plain_text(&detail.blocks);
                        detail.markdown = crate::parser::blocks_to_markdown(&detail.blocks);
                    }
                }
                ui.error(format!("todo failed: {error}"));
            }
            UiMsg::PageEdited {
                page_id,
                generation,
                synced_markdown,
                detail,
            } => {
                ui.tasks.finish("edit", None);
                ui.page_sync_in_flight.remove(&page_id);
                let prev_synced = ui
                    .page_synced_generations
                    .get(&page_id)
                    .copied()
                    .unwrap_or(0);
                ui.page_synced_generations
                    .insert(page_id.clone(), prev_synced.max(generation));

                let current_gen = ui
                    .page_edit_generations
                    .get(&page_id)
                    .copied()
                    .unwrap_or(0);

                if generation >= current_gen {
                    self.merge_page_summary(&detail.page);
                }

                let mut synced_detail = *detail;
                if !crate::parser::has_column_blocks(&synced_detail.blocks)
                    && !crate::page_edit::contains_column_marker(&synced_markdown)
                {
                    synced_detail.markdown = synced_markdown.clone();
                }

                if all_blocks_have_server_ids(&synced_detail.blocks) {
                    ui.page_confirmed_blocks
                        .insert(page_id.clone(), synced_detail.blocks.clone());
                }

                let active_inline_matches = ui
                    .inline_editor
                    .as_ref()
                    .is_some_and(|ed| ed.page_id == page_id);
                let active_block_matches = ui
                    .block_editor
                    .as_ref()
                    .is_some_and(|b| b.page_id == page_id);

                if active_inline_matches {
                    let mut still_dirty = false;
                    if let Some(ed) = ui.inline_editor.as_mut() {
                        ed.sync_in_flight = false;
                        ed.last_sync_error = None;
                        if generation >= ed.synced_generation {
                            ed.synced_generation = generation;
                            ed.remote_blocks = synced_detail.blocks.clone();
                            ed.last_synced_markdown = synced_markdown.clone();
                            ed.last_synced_detail = Some(synced_detail.clone());
                        }
                        still_dirty = ed.is_dirty();
                    }

                    // Only launch a follow-up sync if an explicit save was queued while in flight.
                    if let Some((pending_gen, pending_md, _)) = ui.pending_page_sync.remove(&page_id)
                    {
                        if pending_gen > generation {
                            let known = (!synced_detail.blocks.is_empty())
                                .then_some(synced_detail.blocks);
                            if let Some(ed) = ui.inline_editor.as_mut() {
                                ed.sync_in_flight = true;
                            }
                            ui.page_sync_in_flight.insert(page_id.clone());
                            self.spawn_page_diff_sync(ui, page_id, pending_gen, pending_md, known);
                        }
                    } else if !still_dirty {
                        if self.active_page_id.as_deref() == Some(page_id.as_str()) {
                            self.active_page_detail = Some(synced_detail);
                        }
                        ui.success("Saved to Notion");
                    } else {
                        // User typed newer unsaved edits while explicit save was in flight:
                        // DO NOT overwrite local editor or optimistic detail, and do NOT auto-sync!
                        ui.success("Saved to Notion");
                    }
                } else if active_block_matches {
                    if let Some((pending_gen, pending_md, _)) = ui.pending_page_sync.remove(&page_id)
                    {
                        if pending_gen > generation {
                            let known = (!synced_detail.blocks.is_empty())
                                .then_some(synced_detail.blocks.clone());
                            ui.page_sync_in_flight.insert(page_id.clone());
                            self.spawn_page_diff_sync(ui, page_id.clone(), pending_gen, pending_md, known);
                        }
                    }
                    // Reconcile newly minted server IDs into the live optimistic tree without
                    // losing any unsaved text or freshly created empty sibling block under the cursor.
                    if let Some(ref mut local_detail) = self.active_page_detail {
                        if local_detail.page.id == page_id {
                            reconcile_server_block_ids(
                                &mut local_detail.blocks,
                                &synced_detail.blocks,
                                ui.block_editor.as_mut(),
                            );
                        }
                    }
                    ui.success("Saved to Notion");
                } else if generation < current_gen {
                    if let Some((pending_gen, pending_md, _)) = ui.pending_page_sync.remove(&page_id)
                    {
                        if pending_gen > generation {
                            let known = (!synced_detail.blocks.is_empty())
                                .then_some(synced_detail.blocks);
                            ui.page_sync_in_flight.insert(page_id.clone());
                            self.spawn_page_diff_sync(ui, page_id, pending_gen, pending_md, known);
                        }
                    }
                } else {
                    ui.pending_page_sync.remove(&page_id);
                    if self.active_page_id.as_deref() == Some(page_id.as_str()) {
                        self.active_page_detail = Some(synced_detail);
                    }
                    ui.success("Saved to Notion");
                }
            }
            UiMsg::PageEditFailed {
                page_id,
                generation: _,
                error,
                edited_markdown,
            } => {
                ui.tasks.finish("edit", Some(error.clone()));
                ui.page_sync_in_flight.remove(&page_id);
                let cached = self
                    .storage
                    .load_page_detail(&self.workspace_id, &page_id)
                    .ok()
                    .flatten();
                if let Some(ref actual) = cached {
                    if self.active_page_id.as_deref() == Some(page_id.as_str()) {
                        self.active_page_detail = Some(actual.clone());
                    }
                }
                if let Some(ed) = ui.inline_editor.as_mut().filter(|ed| ed.page_id == page_id) {
                    ed.sync_in_flight = false;
                    ed.last_sync_error = Some(error.clone());
                    if let Some(actual) = cached {
                        ed.remote_blocks = actual.blocks.clone();
                        ed.last_synced_detail = Some(actual);
                    } else {
                        ed.remote_blocks.clear();
                    }
                    ui.error(format!("Sync failed: {error}"));
                } else if ui
                    .block_editor
                    .as_ref()
                    .is_some_and(|b| b.page_id == page_id)
                {
                    ui.error(format!("Sync failed: {error}"));
                } else {
                    self.preserve_failed_page_edit(ui, &page_id, &edited_markdown, &error);
                }
            }
            UiMsg::MediaUrlRefreshed {
                page_id,
                block_id,
                url,
            } => {
                if !block_id.trim().is_empty() && !url.trim().is_empty() {
                    if let Some(ref mut detail) = self.active_page_detail {
                        if page_id.is_empty() || detail.page.id == page_id {
                            for m in &mut detail.media {
                                if m.block_id == block_id {
                                    m.url = url.clone();
                                }
                            }
                            update_block_media_url_in_tree(&mut detail.blocks, &block_id, &url);
                        }
                    }
                    for ov in &mut ui.overlays {
                        if let Overlay::ImagePreview(ref mut preview) = ov {
                            for item in &mut preview.items {
                                if item.block_id == block_id
                                    && (page_id.is_empty() || item.page_id == page_id)
                                {
                                    item.url = url.clone();
                                }
                            }
                        }
                    }
                }
            }
            UiMsg::AppendFailed { page_id, error } => {
                ui.tasks.finish("append", Some(error.clone()));
                if self.active_page_id.as_deref() == Some(page_id.as_str()) {
                    if let Ok(Some(cached)) =
                        self.storage.load_page_detail(&self.workspace_id, &page_id)
                    {
                        self.active_page_detail = Some(cached);
                        self.clamp_canvas_selection(ui);
                    }
                }
                ui.error(format!("append: {error}"));
            }
            UiMsg::PageCreated {
                page,
                preview_detail,
            } => {
                ui.tasks.finish("create", None);
                if let Some(expected) = self.expected_workspace_id.as_deref() {
                    if !page.workspace_id.is_empty() && page.workspace_id != expected {
                        return;
                    }
                }
                let id = page.id.clone();
                let title = page.title.clone();
                let summary = NotionPageSummary::from(&page);
                if let Some(existing) = self.pages.iter_mut().find(|p| p.id == id) {
                    *existing = summary;
                } else {
                    self.pages.insert(0, summary);
                }
                if page.parent_type == "database_id" {
                    let target_db_id = page
                        .parent_id
                        .clone()
                        .or_else(|| self.active_database_id.clone());
                    if let Some(db_id) = target_db_id {
                        if self.active_database_id.as_deref() == Some(db_id.as_str()) {
                            if let Some(existing) =
                                self.active_database_rows.iter_mut().find(|r| r.id == id)
                            {
                                *existing = page;
                            } else {
                                self.active_database_rows.insert(0, page);
                            }
                            self.clamp_db_selection(ui);
                        }
                        self.sync_ctrl
                            .request(SyncCommand::RefreshDatabase { database_id: db_id });
                    }
                } else {
                    let _ = self.try_open_page(&id, ui, true);
                    if let Some(preview) = preview_detail {
                        if self.active_page_id.as_deref() == Some(id.as_str()) {
                            self.active_page_detail = Some(*preview);
                            self.clamp_canvas_selection(ui);
                        }
                    }
                }
                self.snap_sidebar_selection(ui);
                ui.success(format!("Created '{title}'"));
            }
            UiMsg::WorkspaceTreeRefreshed {
                workspace_id,
                pages,
                databases,
                task_label,
                toast,
            } => {
                ui.tasks.finish(task_label, None);
                if self
                    .expected_workspace_id
                    .as_ref()
                    .is_some_and(|expected| *expected != workspace_id)
                {
                    return;
                }
                self.workspace_id = workspace_id;
                self.pages = pages;
                self.databases = databases;
                self.reload_page_parent_hints();
                self.snap_sidebar_selection(ui);
                ui.success(toast);
            }
            UiMsg::PageArchived(id) => {
                ui.tasks.finish("archive", None);
                if let Some(p) = self.pages.iter_mut().find(|p| p.id == id) {
                    p.archived = true;
                }
                for row in &mut self.active_database_rows {
                    if row.id == id {
                        row.archived = true;
                    }
                }
                if let Some(ref mut detail) = self.active_page_detail {
                    if detail.page.id == id {
                        detail.page.archived = true;
                    }
                }
                if ui.inline_editor.as_ref().is_some_and(|ed| ed.page_id == id) {
                    ui.inline_editor = None;
                    ui.cursor_pos = None;
                }
                if ui.block_editor.as_ref().is_some_and(|b| b.page_id == id) {
                    ui.block_editor = None;
                    ui.cursor_pos = None;
                }
                self.snap_sidebar_selection(ui);
                self.clamp_db_selection(ui);
                self.clamp_canvas_selection(ui);
                ui.success("Archived page");
            }
            UiMsg::CommentAdded { page_id, comment } => {
                ui.tasks.finish("comment", None);
                if let Some(ref mut detail) = self.active_page_detail {
                    if detail.page.id == page_id {
                        detail.comments.push(comment);
                    }
                }
                ui.success("Comment posted");
            }
            UiMsg::BoardCardMoved {
                database_id,
                updated,
                column_label,
            } => {
                ui.tasks.finish("move card", None);
                if let Some(row) = self
                    .active_database_rows
                    .iter_mut()
                    .find(|r| r.id == updated.id)
                {
                    *row = updated;
                }
                self.clamp_db_selection(ui);
                if !database_id.is_empty() {
                    self.sync_ctrl
                        .request(SyncCommand::RefreshDatabase { database_id });
                }
                ui.success(format!("Moved card -> {column_label}"));
            }
            UiMsg::BoardCardMoveFailed {
                database_id,
                prev_col,
                prev_row,
                error,
            } => {
                ui.tasks.finish("move card", Some(error.clone()));
                if self.active_database_id.as_deref() == Some(database_id.as_str()) {
                    ui.db_board_col = prev_col;
                    ui.db_board_row = prev_row;
                    self.clamp_db_selection(ui);
                }
                ui.error(format!("move card: {error}"));
            }
            UiMsg::WorkspaceSwitched { name, workspace_id } => {
                ui.tasks.finish("workspace", None);
                self.workspace_name = name.clone();
                self.workspace_id = workspace_id.clone();
                self.expected_workspace_id = Some(workspace_id.clone());
                self.config.set_active_workspace(name.clone());
                if let Ok(new_storage) = Storage::open(&crate::config::db_path(Some(&workspace_id))) {
                    self.storage = new_storage;
                }
                self.active_page_id = None;
                self.active_page_detail = None;
                ui.inline_editor = None;
                ui.block_editor = None;
                ui.canvas_block_layouts.clear();
                ui.canvas_link_hits.clear();
                ui.page_confirmed_blocks.clear();
                ui.cursor_pos = None;
                ui.page_refresh_seq.clear();
                ui.bump_preview_request_seq();
                self.active_database_id = None;
                self.active_database_rows.clear();
                self.active_database_rows_truncated = false;
                self.active_database_rows_fetched = 0;
                self.block_owner_pages.clear();
                self.child_page_positions.clear();
                self.nav_stack.clear();
                // The sync engine must not keep polling a page of the workspace just left.
                self.sync_ctrl.set_focus(FocusTarget::default());
                self.load_cached_initial();
                self.snap_sidebar_selection(ui);
                let rebinding = self
                    .workspace_switch_tx
                    .as_ref()
                    .is_some_and(|tx| tx.send(workspace_id.clone()).is_ok());
                if !rebinding {
                    self.sync_ctrl.request(SyncCommand::RefreshAll);
                }
                ui.success(format!("Switched workspace -> {name}"));
            }
            UiMsg::ImageRendered {
                cache_key,
                rendered,
            } => {
                let sz = rendered.approx_heap_bytes();
                if self.image_cache_bytes + sz > MAX_IMAGE_CACHE_BYTES {
                    self.clear_image_cache();
                }
                self.image_cache_bytes += sz;
                self.image_cache.insert(cache_key, *rendered);
            }
            UiMsg::ImagePreviewFailed { seq, error } => {
                if seq != ui.preview_request_seq {
                    return;
                }
                if let Some(Overlay::ImagePreview(ref mut ov)) = ui.overlays.last_mut() {
                    ov.error = Some(error);
                }
            }
            UiMsg::HtmlPreviewLoaded {
                seq,
                title,
                html,
                mime,
            } => {
                ui.tasks.finish("html", None);
                if seq != ui.preview_request_seq {
                    return;
                }
                if matches!(
                    ui.overlays.last(),
                    Some(Overlay::ImagePreview(_) | Overlay::HtmlPreview(_))
                ) {
                    ui.overlays.pop();
                    ui.active_pixel_graphic = None;
                    ui.pending_pixel_graphics.clear();
                }
                let kind = bitefoundry_html::sniff_content_kind(
                    mime.as_deref(),
                    Some(&title),
                    Some(html.as_bytes()),
                );
                if kind == bitefoundry_html::ContentKind::Markdown {
                    ui.overlays.push(Overlay::markdown_preview(title, html));
                    ui.info("Opened Markdown viewer");
                } else {
                    ui.overlays.push(Overlay::html_preview(title, html));
                    ui.info("Opened interactive HTML viewer");
                }
            }
            UiMsg::HtmlServed { url } => {
                ui.tasks.finish("html-serve", None);
                match open::that(&url) {
                    Ok(()) => ui.success(format!("Served HTML at {url}")),
                    Err(err) => ui.warning(format!("Served HTML at {url} (browser failed: {err})")),
                }
            }
            UiMsg::TaskDone { label, toast } => {
                ui.tasks.finish(&label, None);
                ui.success(toast);
            }
            UiMsg::TaskFailed { label, error } => {
                ui.tasks.finish(&label, Some(error.clone()));
                ui.error(format!("{label}: {error}"));
            }
            UiMsg::BlockChildrenHydrated {
                page_id,
                block_id,
                children,
            } => {
                ui.hydrating_blocks.remove(&block_id);
                if ui.hydrating_blocks.is_empty() {
                    ui.tasks.finish("hydrate", None);
                }
                self.apply_hydrated_children(ui, &page_id, &block_id, children);
            }
            UiMsg::BlockChildrenHydrationFailed { block_id, error } => {
                ui.hydrating_blocks.remove(&block_id);
                ui.tasks.finish("hydrate", Some(error.clone()));
                ui.error(format!("Could not load toggle contents: {error}"));
            }
        }
    }

    /// Fetches, on demand, the children of a block that the recursive page fetch stopped short of
    /// (`has_children` with nothing loaded, e.g. a toggle nested past the tree depth cap), so
    /// expanding it shows its content instead of blank space.
    pub fn hydrate_block_children(&mut self, block_id: &str, ui: &mut UiState) {
        let Some(page_id) = self.active_page_detail.as_ref().map(|d| d.page.id.clone()) else {
            return;
        };
        if block_id.trim().is_empty() || block_id.starts_with("__optimistic_") {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        if !ui.hydrating_blocks.insert(block_id.to_string()) {
            return;
        }
        let client = self.client.clone();
        let ui_tx = self.ui_tx.clone();
        let bid = block_id.to_string();
        ui.tasks.begin("hydrate");
        handle.spawn(async move {
            let fetched = async {
                let mut children = client.list_block_children(&bid, &page_id, Some(&bid)).await?;
                client.hydrate_block_subtrees(&mut children, &page_id).await?;
                Ok::<_, anyhow::Error>(children)
            }
            .await;
            let _ = ui_tx.send(match fetched {
                Ok(children) => UiMsg::BlockChildrenHydrated {
                    page_id,
                    block_id: bid,
                    children,
                },
                Err(err) => UiMsg::BlockChildrenHydrationFailed {
                    block_id: bid,
                    error: err.to_string(),
                },
            });
        });
    }

    /// Attaches lazily fetched children to the open page. The confirmed sync baseline gets the
    /// same children so a later diff-sync sees them as existing content rather than new blocks.
    /// Skipped while the full-page Markdown editor is open: its draft was rendered without them.
    pub fn apply_hydrated_children(
        &mut self,
        ui: &mut UiState,
        page_id: &str,
        block_id: &str,
        children: Vec<NotionBlock>,
    ) -> bool {
        if ui
            .inline_editor
            .as_ref()
            .is_some_and(|ed| ed.page_id == page_id)
        {
            return false;
        }
        let Some(detail) = self
            .active_page_detail
            .as_mut()
            .filter(|d| d.page.id == page_id)
        else {
            return false;
        };
        if !attach_children_in_tree(&mut detail.blocks, block_id, &children) {
            return false;
        }
        if let Some(confirmed) = ui.page_confirmed_blocks.get_mut(page_id) {
            attach_children_in_tree(confirmed, block_id, &children);
        }
        detail.page.plain_text = crate::parser::blocks_to_plain_text(&detail.blocks);
        detail.markdown = crate::parser::blocks_to_markdown(&detail.blocks);
        detail.media = crate::parser::collect_media_refs(&detail.blocks);
        self.clamp_canvas_selection(ui);
        true
    }

    /// Builds the Notion left sidebar entries: Favorites, Workspace hierarchical tree, Databases, Recent / Trash.
    pub fn build_sidebar_items(&self, ui: &UiState) -> Vec<SidebarItem> {
        let mut items = Vec::new();
        let active_pages: Vec<_> = self.pages.iter().filter(|p| !p.archived).collect();
        let trashed_pages: Vec<_> = self.pages.iter().filter(|p| p.archived).collect();

        // 1. Favorites
        let fav_pages: Vec<_> = active_pages.iter().copied().filter(|p| p.is_favorite).collect();
        let fav_dbs: Vec<&NotionDatabase> = self.databases.iter().filter(|d| d.is_favorite).collect();
        if !fav_pages.is_empty() || !fav_dbs.is_empty() {
            items.push(SidebarItem {
                id: "__sec_favorites".into(),
                label: "FAVORITES".into(),
                icon: "★".into(),
                depth: 0,
                is_database: false,
                is_section_header: true,
                collapsible: false,
                collapsed: false,
                is_favorite: true,
                archived: false,
            });
            for p in fav_pages {
                items.push(SidebarItem {
                    id: p.id.clone(),
                    label: p.display_title().to_string(),
                    icon: p.display_icon().to_string(),
                    depth: 0,
                    is_database: false,
                    is_section_header: false,
                    collapsible: false,
                    collapsed: false,
                    is_favorite: true,
                    archived: false,
                });
            }
            for d in fav_dbs {
                items.push(SidebarItem {
                    id: d.id.clone(),
                    label: d.display_title().to_string(),
                    icon: d.display_icon().to_string(),
                    depth: 0,
                    is_database: true,
                    is_section_header: false,
                    collapsible: false,
                    collapsed: false,
                    is_favorite: true,
                    archived: false,
                });
            }
        }

        // 2. Workspace hierarchical tree
        items.push(SidebarItem {
            id: "__sec_workspace".into(),
            label: "WORKSPACE".into(),
            icon: "".into(),
            depth: 0,
            is_database: false,
            is_section_header: true,
            collapsible: false,
            collapsed: false,
            is_favorite: false,
            archived: false,
        });

        // Database rows are never rendered in WORKSPACE, so they cannot be tree parents either: a
        // subpage nested inside a database row is promoted to a root instead of being orphaned
        // under a parent that never appears.
        let page_ids: HashSet<&str> = active_pages
            .iter()
            .filter(|p| p.parent_type != "database_id")
            .map(|p| p.id.as_str())
            .collect();
        let mut effective_parent: HashMap<&str, &str> = HashMap::new();
        for p in &active_pages {
            if p.parent_type == "database_id" {
                continue;
            }
            let parent_page_id: Option<&str> = if p.parent_type == "page_id" {
                p.parent_id.as_deref()
            } else if p.parent_type == "block_id" {
                p.parent_id
                    .as_deref()
                    .and_then(|bid| self.block_owner_pages.get(bid).map(String::as_str))
                    .or_else(|| self.block_owner_pages.get(&p.id).map(String::as_str))
            } else {
                None
            };
            if let Some(pid) = parent_page_id {
                if pid != p.id.as_str() && page_ids.contains(pid) {
                    effective_parent.insert(p.id.as_str(), pid);
                }
            }
        }

        // Break any cycles in parent links by detaching cycle members to roots.
        let mut cycle_nodes: HashSet<&str> = HashSet::new();
        for &start in effective_parent.keys() {
            let mut seen = HashSet::new();
            let mut cur = start;
            while let Some(&next) = effective_parent.get(cur) {
                if !seen.insert(cur) {
                    cycle_nodes.insert(start);
                    break;
                }
                cur = next;
            }
        }

        let mut children_by_parent: HashMap<String, Vec<&NotionPageSummary>> = HashMap::new();
        let mut roots = Vec::new();
        for p in &active_pages {
            // Never dump database rows as top-level WORKSPACE pages (even if DB wasn't synced yet).
            if p.parent_type == "database_id" {
                continue;
            }
            if !cycle_nodes.contains(p.id.as_str()) {
                if let Some(&pid) = effective_parent.get(p.id.as_str()) {
                    children_by_parent
                        .entry(pid.to_string())
                        .or_default()
                        .push(p);
                    continue;
                }
            }
            roots.push(p);
        }

        let is_untitled = |p: &NotionPageSummary| -> bool {
            let t = p.display_title().trim();
            t.is_empty() || t.eq_ignore_ascii_case("untitled")
        };

        for kids in children_by_parent.values_mut() {
            kids.sort_by(|a, b| {
                let pos_a = self
                    .child_page_positions
                    .get(&a.id)
                    .copied()
                    .unwrap_or(i64::MAX);
                let pos_b = self
                    .child_page_positions
                    .get(&b.id)
                    .copied()
                    .unwrap_or(i64::MAX);
                pos_a
                    .cmp(&pos_b)
                    .then_with(|| is_untitled(a).cmp(&is_untitled(b)))
                    .then_with(|| {
                        natural_sort_key(a.display_title())
                            .cmp(&natural_sort_key(b.display_title()))
                    })
                    .then_with(|| a.id.cmp(&b.id))
            });
        }

        roots.sort_by(|a, b| {
            is_untitled(a)
                .cmp(&is_untitled(b))
                .then_with(|| b.is_favorite.cmp(&a.is_favorite))
                .then_with(|| {
                    natural_sort_key(a.display_title()).cmp(&natural_sort_key(b.display_title()))
                })
                .then_with(|| a.id.cmp(&b.id))
        });

        fn push_tree<'a>(
            node: &'a NotionPageSummary,
            depth: usize,
            children_map: &HashMap<String, Vec<&'a NotionPageSummary>>,
            collapsed: &HashSet<String>,
            visited: &mut HashSet<&'a str>,
            out: &mut Vec<SidebarItem>,
        ) {
            if !visited.insert(node.id.as_str()) {
                return;
            }
            let has_kids = children_map
                .get(&node.id)
                .is_some_and(|kids| !kids.is_empty());
            // Past the recursion cap the children cannot be listed, so show the node collapsed.
            let is_collapsed = collapsed.contains(&node.id) || depth >= SIDEBAR_MAX_TREE_DEPTH;
            out.push(SidebarItem {
                id: node.id.clone(),
                label: node.display_title().to_string(),
                icon: node.display_icon().to_string(),
                depth,
                is_database: false,
                is_section_header: false,
                collapsible: has_kids,
                collapsed: is_collapsed,
                is_favorite: node.is_favorite,
                archived: false,
            });
            if has_kids && !is_collapsed {
                if let Some(kids) = children_map.get(&node.id) {
                    for kid in kids {
                        push_tree(kid, depth + 1, children_map, collapsed, visited, out);
                    }
                }
            }
            visited.remove(node.id.as_str());
        }

        let mut visited = HashSet::new();
        for root in roots {
            push_tree(
                root,
                0,
                &children_by_parent,
                &ui.collapsed_nodes,
                &mut visited,
                &mut items,
            );
        }

        // 3. Databases quick section
        if !self.databases.is_empty() {
            items.push(SidebarItem {
                id: "__sec_databases".into(),
                label: "DATABASES".into(),
                icon: "".into(),
                depth: 0,
                is_database: false,
                is_section_header: true,
                collapsible: false,
                collapsed: false,
                is_favorite: false,
                archived: false,
            });
            for db in &self.databases {
                items.push(SidebarItem {
                    id: db.id.clone(),
                    label: db.display_title().to_string(),
                    icon: db.display_icon().to_string(),
                    depth: 0,
                    is_database: true,
                    is_section_header: false,
                    collapsible: false,
                    collapsed: false,
                    is_favorite: db.is_favorite,
                    archived: false,
                });
            }
        }

        // 4. Recent / Trash
        if !trashed_pages.is_empty() {
            items.push(SidebarItem {
                id: "__sec_trash".into(),
                label: format!("TRASH ({})", trashed_pages.len()),
                icon: "🗑".into(),
                depth: 0,
                is_database: false,
                is_section_header: true,
                collapsible: false,
                collapsed: false,
                is_favorite: false,
                archived: true,
            });
            for tp in trashed_pages.into_iter().take(5) {
                items.push(SidebarItem {
                    id: tp.id.clone(),
                    label: tp.display_title().to_string(),
                    icon: tp.display_icon().to_string(),
                    depth: 0,
                    is_database: false,
                    is_section_header: false,
                    collapsible: false,
                    collapsed: false,
                    is_favorite: false,
                    archived: true,
                });
            }
        }

        items
    }

    /// Flattens the active page's block tree into visible interactive blocks respecting expanded toggles.
    pub fn visible_blocks(&self, ui: &UiState) -> Vec<VisibleBlock> {
        let Some(detail) = self.active_page_detail.as_ref() else {
            return Vec::new();
        };
        Self::visible_detail_blocks(detail, ui)
    }

    pub fn visible_detail_blocks(detail: &NotionPageDetail, ui: &UiState) -> Vec<VisibleBlock> {
        let mut out = Vec::new();
        flatten_visible_blocks(&detail.blocks, 0, &ui.expanded_toggles, &mut out);
        out
    }

    /// Returns filtered & sorted rows for the active Database.
    pub fn filtered_sorted_db_rows(&self, ui: &UiState) -> Vec<NotionPage> {
        let q = ui.db_filter.trim().to_ascii_lowercase();
        let mut rows: Vec<NotionPage> = self
            .active_database_rows
            .iter()
            .filter(|r| !r.archived)
            .filter(|r| {
                if q.is_empty() {
                    true
                } else {
                    r.title.to_ascii_lowercase().contains(&q)
                        || r.properties_summary().to_ascii_lowercase().contains(&q)
                }
            })
            .cloned()
            .collect();

        match ui.db_sort {
            DatabaseSortMode::LastEditedDesc => rows.sort_by(|a, b| b.last_edited_time.cmp(&a.last_edited_time)),
            DatabaseSortMode::LastEditedAsc => rows.sort_by(|a, b| a.last_edited_time.cmp(&b.last_edited_time)),
            DatabaseSortMode::TitleAsc => rows.sort_by(|a, b| a.title.to_ascii_lowercase().cmp(&b.title.to_ascii_lowercase())),
            DatabaseSortMode::TitleDesc => rows.sort_by(|a, b| b.title.to_ascii_lowercase().cmp(&a.title.to_ascii_lowercase())),
            DatabaseSortMode::StatusAsc => {
                let status_key = |p: &NotionPage| -> String {
                    p.properties
                        .values()
                        .find_map(|v| match v {
                            NotionPropertyValue::Status(Some(o)) | NotionPropertyValue::Select(Some(o)) => {
                                Some(o.name.to_ascii_lowercase())
                            }
                            _ => None,
                        })
                        .unwrap_or_else(|| "zzz".to_string())
                };
                rows.sort_by_key(|a| status_key(a));
            }
        }
        rows
    }

    /// Groups active Database rows into Kanban columns by `status` (or first `select`) property.
    pub fn build_board_columns(&self, ui: &UiState) -> Vec<BoardColumn> {
        let rows = self.filtered_sorted_db_rows(ui);
        let group_schema = self
            .active_database()
            .and_then(|db| db.board_group_property());

        let (prop_name, prop_type) = match group_schema {
            Some(s) => (s.name.clone(), s.prop_type.clone()),
            None => ("Status".to_string(), "status".to_string()),
        };

        let mut cols: Vec<BoardColumn> = Vec::new();
        if let Some(schema) = group_schema {
            for opt in &schema.options {
                cols.push(BoardColumn {
                    property_name: prop_name.clone(),
                    property_type: prop_type.clone(),
                    label: opt.name.clone(),
                    color: opt.color,
                    cards: Vec::new(),
                    is_synthetic: false,
                });
            }
        }

        for row in rows {
            let (label, color, is_synthetic) = row
                .properties
                .get(&prop_name)
                .and_then(|v| match v {
                    NotionPropertyValue::Status(Some(o)) | NotionPropertyValue::Select(Some(o)) => {
                        Some((o.name.clone(), o.color, false))
                    }
                    _ => None,
                })
                .unwrap_or_else(|| ("No Status".to_string(), NotionColor::Gray, true));

            if let Some(col) = cols
                .iter_mut()
                .find(|c| c.label.eq_ignore_ascii_case(&label))
            {
                col.cards.push(row);
            } else {
                cols.push(BoardColumn {
                    property_name: prop_name.clone(),
                    property_type: prop_type.clone(),
                    label,
                    color,
                    cards: vec![row],
                    is_synthetic,
                });
            }
        }

        if cols.is_empty() {
            cols.push(BoardColumn {
                property_name: prop_name,
                property_type: prop_type,
                label: "Backlog".into(),
                color: NotionColor::Gray,
                cards: Vec::new(),
                is_synthetic: true,
            });
        }
        cols
    }

    /// Breadcrumb trail strings (Workspace / ...history... / Current Item).
    pub fn breadcrumb_labels(&self) -> Vec<String> {
        let mut crumbs = Vec::new();
        for target in &self.nav_stack {
            let label = match target {
                NavTarget::Page(pid) => self
                    .pages
                    .iter()
                    .find(|p| p.id == *pid)
                    .map(|p| format!("{} {}", p.display_icon(), p.display_title()))
                    .unwrap_or_else(|| "Page".into()),
                NavTarget::Database(did) => self
                    .databases
                    .iter()
                    .find(|d| d.id == *did)
                    .map(|d| format!("{} {}", d.display_icon(), d.display_title()))
                    .unwrap_or_else(|| "Database".into()),
            };
            crumbs.push(label);
        }
        crumbs
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum NatSortChunk {
    Text(String),
    Num(u128, usize),
}

fn natural_sort_key(s: &str) -> Vec<NatSortChunk> {
    let mut chunks = Vec::new();
    let mut chars = s.trim().chars().peekable();
    while let Some(&ch) = chars.peek() {
        if ch.is_ascii_digit() {
            let mut num = 0u128;
            let mut len = 0usize;
            while let Some(&d) = chars.peek() {
                if let Some(digit) = d.to_digit(10) {
                    num = num.saturating_mul(10).saturating_add(digit as u128);
                    len += 1;
                    chars.next();
                } else {
                    break;
                }
            }
            chunks.push(NatSortChunk::Num(num, len));
        } else {
            let mut text = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_ascii_digit() {
                    break;
                }
                for lc in c.to_lowercase() {
                    text.push(lc);
                }
                chars.next();
            }
            chunks.push(NatSortChunk::Text(text));
        }
    }
    chunks
}

fn flatten_visible_blocks(
    blocks: &[NotionBlock],
    depth: usize,
    expanded_toggles: &HashSet<String>,
    out: &mut Vec<VisibleBlock>,
) {
    let mut num_idx = 0usize;
    for blk in blocks {
        if matches!(blk.kind, NotionBlockKind::NumberedListItem { .. }) {
            num_idx += 1;
        } else {
            num_idx = 0;
        }

        let is_toggle = matches!(
            blk.kind,
            NotionBlockKind::Toggle { .. }
                | NotionBlockKind::Heading1 { is_toggleable: true, .. }
                | NotionBlockKind::Heading2 { is_toggleable: true, .. }
                | NotionBlockKind::Heading3 { is_toggleable: true, .. }
        );
        let is_open = !is_toggle || expanded_toggles.contains(&blk.id);

        // Tables render their `table_row` children directly inside the table block
        let is_table = matches!(blk.kind, NotionBlockKind::Table { .. });

        // Layout wrappers render no lines of their own: listing them would add invisible cursor
        // stops, so only their children become visible blocks (keeping their indentation).
        if matches!(
            blk.kind,
            NotionBlockKind::ColumnList | NotionBlockKind::Column | NotionBlockKind::SyncedBlock { .. }
        ) {
            flatten_visible_blocks(&blk.children, depth + 1, expanded_toggles, out);
            continue;
        }

        out.push(VisibleBlock {
            block: blk.clone(),
            depth,
            is_toggle_open: is_toggle && expanded_toggles.contains(&blk.id),
            numbered_index: num_idx,
        });

        if !is_table && is_open && !blk.children.is_empty() {
            flatten_visible_blocks(&blk.children, depth + 1, expanded_toggles, out);
        }
    }
}

/// Sets the children of `target_id` when it still has none loaded. Returns whether it was found.
fn attach_children_in_tree(
    blocks: &mut [NotionBlock],
    target_id: &str,
    children: &[NotionBlock],
) -> bool {
    for blk in blocks {
        if blk.id == target_id {
            if blk.children.is_empty() {
                blk.children = children.to_vec();
                blk.has_children = !blk.children.is_empty();
            }
            return true;
        }
        if attach_children_in_tree(&mut blk.children, target_id, children) {
            return true;
        }
    }
    false
}

fn set_todo_in_tree(blocks: &mut [NotionBlock], target_id: &str, checked: bool) -> bool {
    for blk in blocks {
        if blk.id == target_id {
            if let NotionBlockKind::ToDo {
                checked: ref mut c, ..
            } = blk.kind
            {
                *c = checked;
                return true;
            }
        }
        if set_todo_in_tree(&mut blk.children, target_id, checked) {
            return true;
        }
    }
    false
}

fn all_blocks_have_server_ids(blocks: &[NotionBlock]) -> bool {
    blocks.iter().all(|b| {
        !b.id.trim().is_empty()
            && !b.id.starts_with("__optimistic_")
            && all_blocks_have_server_ids(&b.children)
    })
}

fn update_block_in_tree(
    blocks: &mut [NotionBlock],
    target_id: &str,
    new_type: &str,
    new_kind: &NotionBlockKind,
) -> bool {
    for blk in blocks {
        if blk.id == target_id {
            blk.block_type = new_type.to_string();
            blk.kind = new_kind.clone();
            return true;
        }
        if update_block_in_tree(&mut blk.children, target_id, new_type, new_kind) {
            return true;
        }
    }
    false
}

fn find_parent_block_id_in_tree(blocks: &[NotionBlock], target_id: &str) -> Option<String> {
    for blk in blocks {
        if blk.id == target_id {
            return blk.parent_block_id.clone();
        }
        if let Some(found) = find_parent_block_id_in_tree(&blk.children, target_id) {
            return Some(found);
        }
    }
    None
}

fn insert_sibling_after_in_tree(
    blocks: &mut Vec<NotionBlock>,
    after_id: &str,
    new_block: NotionBlock,
) -> bool {
    if let Some(pos) = blocks.iter().position(|b| b.id == after_id) {
        blocks.insert(pos + 1, new_block);
        return true;
    }
    for blk in blocks.iter_mut() {
        if insert_sibling_after_in_tree(&mut blk.children, after_id, new_block.clone()) {
            blk.has_children = true;
            return true;
        }
    }
    false
}

/// Replaces matching `__optimistic_*` block IDs in `local_blocks` with their newly confirmed
/// Notion server IDs from `server_blocks`, keeping `active_editor.block_id` synchronized.
fn reconcile_server_block_ids(
    local_blocks: &mut [NotionBlock],
    server_blocks: &[NotionBlock],
    mut active_editor: Option<&mut InlineBlockEditor>,
) {
    let mut used_server_ids: HashSet<String> = local_blocks
        .iter()
        .filter(|b| !b.id.starts_with("__optimistic_") && !b.id.is_empty())
        .map(|b| b.id.clone())
        .collect();
    for local in local_blocks.iter_mut() {
        if local.id.starts_with("__optimistic_") {
            let local_key = crate::parser::block_comparison_key(local);
            if let Some(srv) = server_blocks.iter().find(|s| {
                !s.id.is_empty()
                    && !used_server_ids.contains(&s.id)
                    && crate::parser::block_comparison_key(s) == local_key
            }) {
                let old_id = local.id.clone();
                local.id = srv.id.clone();
                used_server_ids.insert(srv.id.clone());
                if let Some(ref mut bed) = active_editor {
                    if bed.block_id == old_id {
                        bed.block_id = srv.id.clone();
                    }
                }
            }
        }
        if !local.children.is_empty() {
            if let Some(srv) = server_blocks.iter().find(|s| s.id == local.id) {
                reconcile_server_block_ids(
                    &mut local.children,
                    &srv.children,
                    active_editor.as_deref_mut(),
                );
            }
        }
    }
}

fn update_block_media_url_in_tree(blocks: &mut [NotionBlock], target_id: &str, url: &str) -> bool {
    for blk in blocks {
        if blk.id == target_id {
            match &mut blk.kind {
                NotionBlockKind::Image { url: ref mut u, .. }
                | NotionBlockKind::Video { url: ref mut u, .. }
                | NotionBlockKind::Audio { url: ref mut u, .. }
                | NotionBlockKind::File { url: ref mut u, .. }
                | NotionBlockKind::Pdf { url: ref mut u, .. } => {
                    *u = url.to_string();
                    return true;
                }
                _ => {}
            }
        }
        if update_block_media_url_in_tree(&mut blk.children, target_id, url) {
            return true;
        }
    }
    false
}

fn parse_optimistic_block_payload(
    val: &serde_json::Value,
    page_id: &str,
    parent_block_id: Option<&str>,
    prefix: &str,
    counter: &mut usize,
) -> NotionBlock {
    let mut blk = crate::parser::parse_notion_block(val, page_id, parent_block_id);
    if blk.id.trim().is_empty() {
        blk.id = format!("__optimistic_{prefix}_{page_id}_{}", *counter);
        *counter += 1;
    }
    if blk.children.is_empty() {
        if let Some(nested) = val
            .get(&blk.block_type)
            .and_then(|inner| inner.get("children"))
            .and_then(serde_json::Value::as_array)
        {
            let parent_id = blk.id.clone();
            blk.children = nested
                .iter()
                .map(|child| {
                    parse_optimistic_block_payload(child, page_id, Some(&parent_id), prefix, counter)
                })
                .collect();
        }
    } else {
        for child in &mut blk.children {
            if child.id.trim().is_empty() {
                child.id = format!("__optimistic_{prefix}_{page_id}_{}", *counter);
                *counter += 1;
            }
        }
    }
    if !blk.children.is_empty() {
        blk.has_children = true;
    }
    blk
}

/// Parses quick-append slash or Markdown input into local optimistic `NotionBlock`s with
/// `__optimistic_append_*` IDs (for in-memory display only; never written to SQLite).
pub fn build_optimistic_append_blocks(page_id: &str, markdown_or_slash: &str) -> Vec<NotionBlock> {
    let payloads = crate::parser::parse_slash_command_blocks(markdown_or_slash);
    let mut counter = 0usize;
    payloads
        .iter()
        .map(|val| parse_optimistic_block_payload(val, page_id, None, "append", &mut counter))
        .collect()
}

/// Builds an immediate local `NotionPageDetail` preview for a newly created page from the
/// user's Markdown body so the canvas renders right away while background sync hydrates real IDs.
pub fn build_created_page_preview(mut page: NotionPage, markdown_body: &str) -> NotionPageDetail {
    let page_id = page.id.clone();
    let blocks = if markdown_body.trim().is_empty() {
        Vec::new()
    } else {
        let payloads = crate::parser::markdown_to_notion_blocks(markdown_body);
        let mut counter = 0usize;
        payloads
            .iter()
            .map(|val| parse_optimistic_block_payload(val, &page_id, None, "create", &mut counter))
            .collect()
    };
    page.plain_text = crate::parser::blocks_to_plain_text(&blocks);
    let markdown = crate::parser::blocks_to_markdown(&blocks);
    let media = crate::parser::collect_media_refs(&blocks);
    NotionPageDetail {
        page,
        blocks,
        comments: Vec::new(),
        comments_error: None,
        markdown,
        media,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app_and_ui() -> (AppState, UiState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(&dir.path().join("ui_app.db")).unwrap();
        let cfg = AppConfig::default();
        let auth = AuthManager::from_token(cfg.clone(), "ntn_ui_test");
        let client = NotionClient::new(&cfg, auth.clone()).unwrap();
        let (focus_tx, _focus_rx) = tokio::sync::watch::channel(FocusTarget::default());
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
        let app = AppState::new(cfg, auth, client, storage, sync_ctrl, ui_tx);
        let ui = UiState::new();
        (app, ui, dir)
    }

    #[test]
    fn page_created_renders_preview_immediately_without_persisting_optimistic_ids() {
        let (mut app, mut ui, _dir) = test_app_and_ui();
        let page = NotionPage {
            id: "page-created-1".into(),
            workspace_id: app.workspace_id.clone(),
            parent_type: "workspace".into(),
            title: "Launch Plan".into(),
            ..Default::default()
        };
        app.storage.upsert_page(&page).unwrap();

        let preview = build_created_page_preview(page.clone(), "# Kickoff\n\nShip v1");
        app.handle_ui_msg(
            &mut ui,
            UiMsg::PageCreated {
                page: page.clone(),
                preview_detail: Some(Box::new(preview)),
            },
        );

        let active = app
            .active_page_detail
            .as_ref()
            .expect("active_page_detail should be populated immediately");
        assert_eq!(active.page.id, "page-created-1");
        assert_eq!(active.blocks.len(), 2);
        assert!(active.blocks.iter().all(|b| b.id.starts_with("__optimistic_create_")));
        assert!(active.markdown.contains("Kickoff"));
        assert!(active.markdown.contains("Ship v1"));

        // SQLite must NEVER contain synthetic __optimistic_* blocks.
        assert!(
            app.storage
                .load_cached_children(&app.workspace_id, "page-created-1")
                .unwrap()
                .is_none(),
            "optimistic preview must stay in memory only"
        );
    }

    #[test]
    fn optimistic_append_renders_immediately_and_reverts_on_failure() {
        let (mut app, mut ui, _dir) = test_app_and_ui();
        let page = NotionPage {
            id: "page-append-1".into(),
            workspace_id: app.workspace_id.clone(),
            parent_type: "workspace".into(),
            title: "Notes".into(),
            ..Default::default()
        };
        let existing = NotionBlock {
            id: "blk-real-1".into(),
            page_id: "page-append-1".into(),
            parent_block_id: None,
            block_type: "paragraph".into(),
            has_children: false,
            archived: false,
            kind: NotionBlockKind::Paragraph {
                rich_text: vec![crate::models::RichTextSpan::plain("Original")],
                color: NotionColor::Default,
            },
            children: Vec::new(),
        };
        app.storage
            .save_page_detail(&NotionPageDetail {
                page: page.clone(),
                blocks: vec![existing],
                comments: Vec::new(),
                comments_error: None,
                markdown: "Original".into(),
                media: Vec::new(),
            })
            .unwrap();
        app.pages.push(NotionPageSummary::from(&page));
        app.open_page("page-append-1", &mut ui, false);

        assert!(app.apply_optimistic_append_local("page-append-1", "/todo Ship now", &mut ui));
        let after_opt = app.active_page_detail.as_ref().unwrap();
        assert_eq!(after_opt.blocks.len(), 2);
        assert!(after_opt.blocks[1].id.starts_with("__optimistic_append_"));
        assert!(after_opt.markdown.contains("Ship now"));

        // SQLite still holds only the single server-confirmed block.
        let cached = app
            .storage
            .load_page_detail(&app.workspace_id, "page-append-1")
            .unwrap()
            .unwrap();
        assert_eq!(cached.blocks.len(), 1);

        // Failure reverts in-memory canvas back to SQLite's authoritative block list.
        app.handle_ui_msg(
            &mut ui,
            UiMsg::AppendFailed {
                page_id: "page-append-1".into(),
                error: "network timeout".into(),
            },
        );
        let reverted = app.active_page_detail.as_ref().unwrap();
        assert_eq!(reverted.blocks.len(), 1);
        assert_eq!(reverted.blocks[0].id, "blk-real-1");
    }

    fn sidebar_page(id: &str, parent_type: &str, parent_id: Option<&str>) -> NotionPageSummary {
        NotionPageSummary {
            id: id.into(),
            title: id.into(),
            parent_type: parent_type.into(),
            parent_id: parent_id.map(str::to_string),
            ..Default::default()
        }
    }

    fn sidebar_index(items: &[SidebarItem], id: &str) -> Option<usize> {
        items.iter().position(|it| it.id == id && !it.is_section_header)
    }

    #[test]
    fn sidebar_lists_depth_seven_page_with_capped_indent_and_clickable_toggle() {
        use crate::ui::action::{dispatch, Action};
        use crate::ui::hitmap::HitTarget;

        let (mut app, mut ui, _dir) = test_app_and_ui();
        // page-depth-0 (workspace) -> page-depth-1 -> ... -> page-depth-8
        app.pages = (0..=8)
            .map(|d| {
                let parent = (d > 0).then(|| format!("page-depth-{}", d - 1));
                let parent_type = if d == 0 { "workspace" } else { "page_id" };
                sidebar_page(&format!("page-depth-{d}"), parent_type, parent.as_deref())
            })
            .collect();

        let items = app.build_sidebar_items(&ui);
        for d in 0..=8 {
            let idx = sidebar_index(&items, &format!("page-depth-{d}"))
                .unwrap_or_else(|| panic!("depth-{d} page must be listed"));
            assert_eq!(items[idx].depth, d);
        }
        let d7 = sidebar_index(&items, "page-depth-7").unwrap();
        assert!(items[d7].collapsible && !items[d7].collapsed);

        // Indentation stops growing at the cap, and the arrow hit rect sits on the drawn arrow.
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(48, 20)).unwrap();
        let mut hitmap = HitMap::new();
        terminal
            .draw(|f| crate::ui::nav_pane::draw(f, f.area(), &app, &ui, &mut hitmap))
            .unwrap();
        let arrow = hitmap
            .region_of(HitTarget::SidebarToggle(d7))
            .expect("depth-7 arrow must be clickable");
        let capped = (crate::ui::nav_pane::SIDEBAR_MAX_INDENT_LEVEL * 2) as u16;
        assert_eq!(arrow.x, capped);
        let buf = terminal.backend().buffer();
        assert_eq!(buf[(arrow.x + 1, arrow.y)].symbol(), "▾");
        let d8_row = hitmap
            .region_of(HitTarget::SidebarRow(sidebar_index(&items, "page-depth-8").unwrap()))
            .unwrap();
        assert_eq!(buf[(capped + 1, d8_row.y)].symbol(), "└");

        // Clicking the depth-7 arrow collapses its subtree; clicking again expands it.
        let click = |app: &mut AppState, ui: &mut UiState| {
            dispatch(
                Action::MouseClick {
                    target: HitTarget::SidebarToggle(d7),
                    col: arrow.x + 1,
                    row: arrow.y,
                },
                app,
                ui,
            );
        };
        click(&mut app, &mut ui);
        assert!(ui.collapsed_nodes.contains("page-depth-7"));
        let collapsed = app.build_sidebar_items(&ui);
        assert!(collapsed[sidebar_index(&collapsed, "page-depth-7").unwrap()].collapsed);
        assert!(sidebar_index(&collapsed, "page-depth-8").is_none());

        click(&mut app, &mut ui);
        assert!(!ui.collapsed_nodes.contains("page-depth-7"));
        let expanded = app.build_sidebar_items(&ui);
        assert_eq!(expanded[sidebar_index(&expanded, "page-depth-8").unwrap()].depth, 8);
    }

    #[test]
    fn sidebar_tree_depth_cap_still_breaks_parent_cycles() {
        let (mut app, ui, _dir) = test_app_and_ui();
        // A 30-level chain is listed up to the cap; a parent cycle must not recurse forever.
        app.pages = (0..30)
            .map(|d| {
                let parent = (d > 0).then(|| format!("deep-{}", d - 1));
                let parent_type = if d == 0 { "workspace" } else { "page_id" };
                sidebar_page(&format!("deep-{d}"), parent_type, parent.as_deref())
            })
            .chain([
                sidebar_page("cycle-a", "page_id", Some("cycle-b")),
                sidebar_page("cycle-b", "page_id", Some("cycle-a")),
            ])
            .collect();

        let items = app.build_sidebar_items(&ui);
        let last = sidebar_index(&items, &format!("deep-{SIDEBAR_MAX_TREE_DEPTH}")).unwrap();
        assert_eq!(items[last].depth, SIDEBAR_MAX_TREE_DEPTH);
        assert!(items[last].collapsible && items[last].collapsed);
        assert!(sidebar_index(&items, &format!("deep-{}", SIDEBAR_MAX_TREE_DEPTH + 1)).is_none());
        assert!(sidebar_index(&items, "cycle-a").is_some());
        assert!(sidebar_index(&items, "cycle-b").is_some());
    }

    #[test]
    fn sidebar_lists_subpages_nested_inside_database_rows() {
        let (mut app, ui, _dir) = test_app_and_ui();
        app.pages = vec![
            sidebar_page("page-root", "workspace", None),
            sidebar_page("db-row-1", "database_id", Some("db-1")),
            sidebar_page("page-in-row", "page_id", Some("db-row-1")),
            sidebar_page("page-in-row-child", "page_id", Some("page-in-row")),
            sidebar_page("page-in-row-block", "block_id", Some("blk-in-row")),
        ];
        app.block_owner_pages
            .insert("blk-in-row".into(), "db-row-1".into());

        let items = app.build_sidebar_items(&ui);
        let workspace = items
            .iter()
            .position(|it| it.id == "__sec_workspace")
            .expect("WORKSPACE section");
        let pos = |id: &str| {
            sidebar_index(&items, id).unwrap_or_else(|| panic!("{id} must not be orphaned"))
        };
        assert!(sidebar_index(&items, "db-row-1").is_none(), "database rows stay out of WORKSPACE");
        for id in ["page-in-row", "page-in-row-block"] {
            assert!(pos(id) > workspace);
            assert_eq!(items[pos(id)].depth, 0);
        }
        assert!(items[pos("page-in-row")].collapsible);
        assert_eq!(items[pos("page-in-row-child")].depth, 1);
        assert_eq!(pos("page-in-row-child"), pos("page-in-row") + 1);
    }

    fn para(id: &str, text: &str) -> NotionBlock {
        NotionBlock {
            id: id.into(),
            block_type: "paragraph".into(),
            kind: NotionBlockKind::Paragraph {
                rich_text: vec![RichTextSpan::plain(text)],
                color: NotionColor::Default,
            },
            ..Default::default()
        }
    }

    fn wrapper(id: &str, block_type: &str, kind: NotionBlockKind, children: Vec<NotionBlock>) -> NotionBlock {
        NotionBlock {
            id: id.into(),
            block_type: block_type.into(),
            has_children: true,
            kind,
            children,
            ..Default::default()
        }
    }

    #[test]
    fn flatten_visible_blocks_skips_zero_line_container_wrappers() {
        let blocks = vec![
            wrapper(
                "cl",
                "column_list",
                NotionBlockKind::ColumnList,
                vec![
                    wrapper("col-1", "column", NotionBlockKind::Column, vec![para("a", "A")]),
                    wrapper("col-2", "column", NotionBlockKind::Column, vec![para("b", "B")]),
                ],
            ),
            wrapper(
                "sync",
                "synced_block",
                NotionBlockKind::SyncedBlock {
                    synced_from_block_id: None,
                },
                vec![para("c", "C")],
            ),
            para("d", "D"),
        ];
        let mut out = Vec::new();
        flatten_visible_blocks(&blocks, 0, &HashSet::new(), &mut out);
        let stops: Vec<(&str, usize)> =
            out.iter().map(|v| (v.block.id.as_str(), v.depth)).collect();
        assert_eq!(stops, vec![("a", 2), ("b", 2), ("c", 1), ("d", 0)]);
    }

    #[test]
    fn hydrated_toggle_children_attach_to_detail_and_confirmed_baseline() {
        let (mut app, mut ui, _dir) = test_app_and_ui();
        let deep_toggle = NotionBlock {
            id: "blk-deep-toggle".into(),
            block_type: "toggle".into(),
            has_children: true,
            kind: NotionBlockKind::Toggle {
                rich_text: vec![RichTextSpan::plain("Deep")],
                color: NotionColor::Default,
            },
            ..Default::default()
        };
        let outer = wrapper(
            "blk-outer",
            "toggle",
            NotionBlockKind::Toggle {
                rich_text: vec![RichTextSpan::plain("Outer")],
                color: NotionColor::Default,
            },
            vec![deep_toggle],
        );
        let blocks = vec![outer];
        app.active_page_detail = Some(NotionPageDetail {
            page: NotionPage {
                id: "page-hydrate".into(),
                ..Default::default()
            },
            markdown: crate::parser::blocks_to_markdown(&blocks),
            blocks: blocks.clone(),
            comments: Vec::new(),
            comments_error: None,
            media: Vec::new(),
        });
        ui.page_confirmed_blocks
            .insert("page-hydrate".into(), blocks);
        ui.hydrating_blocks.insert("blk-deep-toggle".into());
        ui.expanded_toggles.insert("blk-outer".into());
        ui.expanded_toggles.insert("blk-deep-toggle".into());

        app.handle_ui_msg(
            &mut ui,
            UiMsg::BlockChildrenHydrated {
                page_id: "page-hydrate".into(),
                block_id: "blk-deep-toggle".into(),
                children: vec![para("blk-lazy", "Lazy child")],
            },
        );

        assert!(ui.hydrating_blocks.is_empty());
        let detail = app.active_page_detail.as_ref().unwrap();
        assert_eq!(detail.blocks[0].children[0].children[0].id, "blk-lazy");
        assert!(detail.markdown.contains("Lazy child"));
        let confirmed = &ui.page_confirmed_blocks["page-hydrate"];
        assert_eq!(confirmed[0].children[0].children[0].id, "blk-lazy");
        let visible: Vec<String> = app
            .visible_blocks(&ui)
            .into_iter()
            .map(|v| v.block.id)
            .collect();
        assert_eq!(visible, vec!["blk-outer", "blk-deep-toggle", "blk-lazy"]);
    }
}
