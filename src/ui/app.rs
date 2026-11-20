//! Notion TUI application state (`AppState`), UI presentation state (`UiState`),
//! sidebar tree builder, visible block flattener (with live toggle expansion),
//! and database Table / Board (Kanban) / List query helpers.

use std::collections::{HashMap, HashSet};

use bitefoundry_html::NativeHtmlServer;
use ratatui::layout::Rect;
use tokio::sync::mpsc;

use crate::{
    auth::AuthManager,
    config::{AppConfig, DensityMode, PageWidthMode},
    img_preview::{
        RenderedTerminalImage, TerminalGraphicsCaps, TerminalImageProtocol, MAX_IMAGE_CACHE_BYTES,
    },
    models::{
        CoreEvent, DatabaseSortMode, DatabaseViewMode, NotionBlock, NotionBlockKind, NotionColor,
        NotionComment, NotionDatabase, NotionPage, NotionPageDetail, NotionPropertyValue,
        NotionUser, POLL_RETRY_SUMMARY,
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
}

pub enum UiMsg {
    StartLocalBootstrap,
    DaemonAttached {
        pid: Option<u32>,
        sync_busy: bool,
    },
    DaemonDetached,
    PageRefreshed(Box<NotionPageDetail>),
    PageEdited(Box<NotionPageDetail>),
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
    WorkspaceSwitched {
        name: String,
        workspace_id: String,
    },
    ImageRendered {
        cache_key: String,
        rendered: Box<RenderedTerminalImage>,
    },
    ImagePreviewFailed(String),
    HtmlPreviewLoaded {
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
}

pub struct AppState {
    pub config: AppConfig,
    pub auth: AuthManager,
    pub client: NotionClient,
    pub storage: Storage,
    pub sync_ctrl: SyncController,
    pub ui_tx: mpsc::UnboundedSender<UiMsg>,
    pub workspace_id: String,
    pub workspace_name: String,
    pub current_user: Option<NotionUser>,
    pub pages: Vec<NotionPage>,
    pub databases: Vec<NotionDatabase>,
    pub active_view: CanvasView,
    pub active_page_id: Option<String>,
    pub active_page_detail: Option<NotionPageDetail>,
    pub active_database_id: Option<String>,
    pub active_database_rows: Vec<NotionPage>,
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
            workspace_id,
            workspace_name,
            current_user: None,
            pages: Vec::new(),
            databases: Vec::new(),
            active_view: CanvasView::Page,
            active_page_id: None,
            active_page_detail: None,
            active_database_id: None,
            active_database_rows: Vec::new(),
            nav_stack: Vec::new(),
            syncing: false,
            sync_summary: "Ready".to_string(),
            image_cache: HashMap::new(),
            image_cache_bytes: 0,
            html_server: NativeHtmlServer::new(),
        }
    }

    pub fn load_cached_initial(&mut self) {
        if let Ok(pages) = self.storage.list_pages(&self.workspace_id, true) {
            self.pages = pages;
        }
        if let Ok(dbs) = self.storage.list_databases(&self.workspace_id) {
            self.databases = dbs;
        }
        if self.active_page_id.is_none() && self.active_database_id.is_none() {
            if let Some(first_page) = self.pages.iter().find(|p| !p.archived).cloned() {
                self.active_view = CanvasView::Page;
                self.active_page_id = Some(first_page.id.clone());
                self.active_page_detail = self
                    .storage
                    .load_page_detail(&self.workspace_id, &first_page.id)
                    .ok()
                    .flatten();
            } else if let Some(first_db) = self.databases.first().cloned() {
                self.active_view = CanvasView::Database;
                self.active_database_id = Some(first_db.id.clone());
                self.active_database_rows = self
                    .storage
                    .list_database_rows(&self.workspace_id, &first_db.id)
                    .unwrap_or_default();
            }
        }
    }

    pub fn active_database(&self) -> Option<&NotionDatabase> {
        let id = self.active_database_id.as_deref()?;
        self.databases.iter().find(|d| d.id == id)
    }

    pub fn open_page(&mut self, page_id: &str, ui: &mut UiState, push_history: bool) {
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
        } else if let Some(p) = self.pages.iter().find(|p| p.id == page_id).cloned() {
            self.active_page_detail = Some(NotionPageDetail {
                page: p,
                ..Default::default()
            });
        }
        self.sync_ctrl.set_focus(FocusTarget {
            page_id: Some(page_id.to_string()),
            database_id: None,
        });
        self.sync_ctrl.request(SyncCommand::RefreshPage {
            page_id: page_id.to_string(),
        });
    }

    pub fn open_database(&mut self, database_id: &str, ui: &mut UiState, push_history: bool) {
        if push_history {
            self.push_current_to_nav_stack();
        }
        self.active_view = CanvasView::Database;
        self.active_database_id = Some(database_id.to_string());
        ui.db_selected_row = 0;
        ui.db_board_col = 0;
        ui.db_board_row = 0;
        ui.canvas_scroll = 0;

        self.active_database_rows = self
            .storage
            .list_database_rows(&self.workspace_id, database_id)
            .unwrap_or_default();
        self.sync_ctrl.set_focus(FocusTarget {
            page_id: None,
            database_id: Some(database_id.to_string()),
        });
        self.sync_ctrl.request(SyncCommand::RefreshDatabase {
            database_id: database_id.to_string(),
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
        }
        let client = self.client.clone();
        let storage = self.storage.clone();
        let ui_tx = self.ui_tx.clone();
        let bid = block_id.to_string();
        let page_id = self.active_page_id.clone().unwrap_or_default();
        ui.tasks.begin("todo");
        tokio::spawn(async move {
            match client.set_todo_checked(&bid, &page_id, next_checked).await {
                Ok(_) => {
                    if let Ok(detail) = crate::sync::sync_page_detail(&client, &storage, &page_id).await {
                        let _ = ui_tx.send(UiMsg::PageRefreshed(Box::new(detail)));
                    } else {
                        let _ = ui_tx.send(UiMsg::TaskDone {
                            label: "todo".into(),
                            toast: "Updated to-do".into(),
                        });
                    }
                }
                Err(err) => {
                    let _ = ui_tx.send(UiMsg::TaskFailed {
                        label: "todo".into(),
                        error: err.to_string(),
                    });
                }
            }
        });
    }

    pub fn handle_core_event(&mut self, ui: &mut UiState, ev: CoreEvent) {
        match ev {
            CoreEvent::ProfileLoaded(user) => {
                if let Some(ref ws_name) = user.workspace_name {
                    if !ws_name.trim().is_empty() {
                        self.workspace_name = ws_name.clone();
                    }
                }
                self.current_user = Some(user);
            }
            CoreEvent::WorkspaceTreeUpdated {
                workspace_id,
                pages,
                databases,
            } => {
                self.workspace_id = workspace_id;
                self.pages = pages;
                self.databases = databases;
                if self.active_page_id.is_none() && self.active_database_id.is_none() {
                    if let Some(first) = self.pages.iter().find(|p| !p.archived).cloned() {
                        self.open_page(&first.id, ui, false);
                    } else if let Some(first_db) = self.databases.first().cloned() {
                        self.open_database(&first_db.id, ui, false);
                    }
                }
            }
            CoreEvent::PageDetailUpdated(detail) => {
                if self.active_page_id.as_deref() == Some(detail.page.id.as_str()) {
                    self.active_page_detail = Some(*detail);
                }
            }
            CoreEvent::DatabaseRowsUpdated {
                database_id,
                database,
                rows,
            } => {
                if let Some(db) = database {
                    if let Some(existing) = self.databases.iter_mut().find(|d| d.id == db.id) {
                        *existing = db;
                    } else {
                        self.databases.push(db);
                    }
                }
                if self.active_database_id.as_deref() == Some(database_id.as_str()) {
                    self.active_database_rows = rows;
                }
            }
            CoreEvent::CommentsUpdated { page_id, comments } => {
                if let Some(ref mut detail) = self.active_page_detail {
                    if detail.page.id == page_id {
                        detail.comments = comments;
                    }
                }
            }
            CoreEvent::SyncStateChanged { busy, summary } => {
                self.syncing = busy;
                ui.poll_retrying = summary == POLL_RETRY_SUMMARY;
                self.sync_summary = summary;
            }
            CoreEvent::SyncError(err) => {
                ui.error(format!("Sync: {err}"));
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
            UiMsg::PageRefreshed(detail) => {
                ui.tasks.finish("append", None);
                ui.tasks.finish("todo", None);
                self.active_page_detail = Some(*detail);
                ui.success("Synced page");
            }
            UiMsg::PageEdited(detail) => {
                ui.tasks.finish("edit", None);
                if let Some(existing) = self.pages.iter_mut().find(|p| p.id == detail.page.id) {
                    *existing = detail.page.clone();
                }
                self.active_page_detail = Some(*detail);
                ui.success("Saved changes to Notion");
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
                let id = page.id.clone();
                let title = page.title.clone();
                self.pages.insert(0, page);
                if self.active_view == CanvasView::Database {
                    if let Some(db_id) = self.active_database_id.clone() {
                        self.sync_ctrl
                            .request(SyncCommand::RefreshDatabase { database_id: db_id });
                    }
                } else {
                    self.open_page(&id, ui, true);
                }
                ui.success(format!("Created '{title}'"));
            }
            UiMsg::PageArchived(id) => {
                ui.tasks.finish("archive", None);
                if let Some(p) = self.pages.iter_mut().find(|p| p.id == id) {
                    p.archived = true;
                }
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
                if !database_id.is_empty() {
                    self.sync_ctrl
                        .request(SyncCommand::RefreshDatabase { database_id });
                }
                ui.success(format!("Moved card -> {column_label}"));
            }
            UiMsg::WorkspaceSwitched { name, workspace_id } => {
                ui.tasks.finish("workspace", None);
                self.workspace_name = name.clone();
                self.workspace_id = workspace_id.clone();
                self.config.active_workspace = name.clone();
                if let Ok(new_storage) = Storage::open(&crate::config::db_path(Some(&workspace_id))) {
                    self.storage = new_storage;
                }
                self.active_page_id = None;
                self.active_page_detail = None;
                self.active_database_id = None;
                self.active_database_rows.clear();
                self.nav_stack.clear();
                self.load_cached_initial();
                self.sync_ctrl.request(SyncCommand::RefreshAll);
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
            UiMsg::ImagePreviewFailed(err) => {
                if let Some(Overlay::ImagePreview(ref mut ov)) = ui.overlays.last_mut() {
                    ov.error = Some(err);
                }
            }
            UiMsg::HtmlPreviewLoaded { title, html } => {
                ui.tasks.finish("html", None);
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
        let active_pages: Vec<&NotionPage> = self.pages.iter().filter(|p| !p.archived).collect();
        let trashed_pages: Vec<&NotionPage> = self.pages.iter().filter(|p| p.archived).collect();

        // 1. Favorites
        let fav_pages: Vec<&NotionPage> = active_pages.iter().copied().filter(|p| p.is_favorite).collect();
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
        let db_ids: HashSet<&str> = self.databases.iter().map(|d| d.id.as_str()).collect();
        let mut children_by_parent: HashMap<String, Vec<&NotionPage>> = HashMap::new();
        let mut roots: Vec<&NotionPage> = Vec::new();
        for p in &active_pages {
            // Skip database rows in the top-level page tree (they live inside their database)
            if p.parent_type == "database_id"
                && p.parent_id
                    .as_deref()
                    .is_some_and(|pid| db_ids.contains(pid))
            {
                continue;
            }
            if p.parent_type == "page_id" {
                if let Some(ref pid) = p.parent_id {
                    if page_ids.contains(pid.as_str()) {
                        children_by_parent.entry(pid.clone()).or_default().push(p);
                        continue;
                    }
                }
            }
            roots.push(p);
        }

        fn push_tree(
            node: &NotionPage,
            depth: usize,
            children_map: &HashMap<String, Vec<&NotionPage>>,
            collapsed: &HashSet<String>,
            out: &mut Vec<SidebarItem>,
        ) {
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
                        push_tree(kid, depth + 1, children_map, collapsed, out);
                    }
                }
            }
        }

        for root in roots {
            push_tree(root, 0, &children_by_parent, &ui.collapsed_nodes, &mut items);
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
                });
            }
        }

        for row in rows {
            let (label, color) = row
                .properties
                .get(&prop_name)
                .and_then(|v| match v {
                    NotionPropertyValue::Status(Some(o)) | NotionPropertyValue::Select(Some(o)) => {
                        Some((o.name.clone(), o.color))
                    }
                    _ => None,
                })
                .unwrap_or_else(|| ("No Status".to_string(), NotionColor::Gray));

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
