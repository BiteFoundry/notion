//! Core TUI application state (`AppState`), presentation state (`UiState`), and navigation.

use std::collections::{HashMap, HashSet};

use bitefoundry_html::NativeHtmlServer;
use ratatui::layout::Rect;
use tokio::sync::mpsc;

use crate::{
    auth::AuthManager,
    config::{AppConfig, DensityMode, PageWidthMode},
    img_preview::{RenderedTerminalImage, TerminalGraphicsCaps, TerminalImageProtocol},
    models::{
        DatabaseSortMode, DatabaseViewMode, NotionBlock, NotionColor, NotionComment, NotionDatabase,
        NotionPage, NotionPageDetail, NotionPageSummary, NotionUser,
    },
    notion::NotionClient,
    storage::Storage,
    sync::{FocusTarget, SyncCommand, SyncController},
    ui::{
        hitmap::HitMap,
        keymap::KeyMap,
        overlays::Overlay,
        theme::Theme,
        toast::{TaskBoard, ToastQueue},
    },
};

use super::editor_state::{CanvasBlockLayout, CanvasLinkHit, InlineBlockEditor, InlinePageEditor};

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
    pub(super) image_cache_bytes: usize,
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

    pub(super) fn merge_page_summary(&mut self, page: &NotionPage) -> bool {
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


    pub(super) fn push_current_to_nav_stack(&mut self) {
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

}
