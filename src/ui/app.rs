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
        CoreEvent, DatabaseSortMode, DatabaseViewMode, NotionBlock, NotionBlockKind, NotionColor,
        NotionComment, NotionDatabase, NotionPage, NotionPageDetail, NotionPageSummary, NotionPropertyValue,
        NotionUser, POLL_RETRY_SUMMARY,
    },
    notion::NotionClient,
    parser::apply_optimistic_markdown_to_detail,
    storage::Storage,
    sync::{FocusTarget, SyncCommand, SyncController},
    ui::{
        composer::{normalize, MultiLineComposer},
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
}

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
    PageCreated(NotionPage),
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
    pub pending_pixel_graphic: Option<PendingPixelGraphic>,
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
            pending_pixel_graphic: None,
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
        if self.inline_editor.is_some() && self.focus == Focus::Canvas {
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
            nav_stack: Vec::new(),
            syncing: false,
            sync_summary: "Ready".to_string(),
            image_cache: HashMap::new(),
            image_cache_bytes: 0,
            html_server: NativeHtmlServer::new(),
        }
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
            ui.canvas_follow_selected = false;
        }
        ui.cursor_pos = None;
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
        let client = self.client.clone();
        let storage = self.storage.clone();
        let ui_tx = self.ui_tx.clone();
        ui.tasks.begin("edit");
        tokio::spawn(async move {
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
        ui.tasks.begin("todo");
        tokio::spawn(async move {
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
                    .is_some_and(|ed| ed.page_id == detail.page.id);
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

                let active_inline_matches = ui
                    .inline_editor
                    .as_ref()
                    .is_some_and(|ed| ed.page_id == page_id);

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
            UiMsg::PageCreated(page) => {
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
                }
                self.snap_sidebar_selection(ui);
                ui.success(format!("Created '{title}'"));
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
                ui.cursor_pos = None;
                ui.page_refresh_seq.clear();
                ui.bump_preview_request_seq();
                self.active_database_id = None;
                self.active_database_rows.clear();
                self.active_database_rows_truncated = false;
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
                    self.image_cache.clear();
                    self.image_cache_bytes = 0;
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
            UiMsg::HtmlPreviewLoaded { seq, title, html } => {
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
                    ui.pending_pixel_graphic = None;
                }
                ui.overlays.push(Overlay::html_preview(title, html));
                ui.info("Opened interactive HTML viewer");
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
        }
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

        let page_ids: HashSet<&str> = active_pages.iter().map(|p| p.id.as_str()).collect();
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
            let is_collapsed = collapsed.contains(&node.id);
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
            if has_kids && !is_collapsed && depth < 5 {
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
