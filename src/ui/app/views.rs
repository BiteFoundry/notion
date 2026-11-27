//! Sidebar tree builder, visible block flattener, database query/Kanban views, and block tree helpers.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use crate::models::{
    DatabaseSortMode, NotionBlock, NotionBlockKind, NotionColor, NotionDatabase, NotionPage,
    NotionPageDetail, NotionPageSummary, NotionPropertyValue,
};

use super::{
    editor_state::InlineBlockEditor,
    state::{
        AppState, BoardColumn, NavTarget, SidebarItem, UiState, VisibleBlock,
        SIDEBAR_MAX_TREE_DEPTH,
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct SidebarCacheKey {
    pages_gen: u64,
    dbs_gen: u64,
    collapsed_gen: u64,
    workspace_id: String,
    pages_len: usize,
    pages_ptr: usize,
    dbs_len: usize,
    dbs_ptr: usize,
    block_owners_len: usize,
    child_positions_len: usize,
    collapsed_len: usize,
    collapsed_hash: u64,
}

pub(super) struct CachedSidebarItems {
    key: SidebarCacheKey,
    items: Arc<[SidebarItem]>,
}

pub(crate) fn hash_string_set(set: &HashSet<String>) -> u64 {
    let mut acc = 0u64;
    for s in set {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for b in s.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        acc ^= h;
    }
    acc
}

fn ascii_contains_ignore_case(haystack: &str, needle_lower: &str) -> bool {
    let n = needle_lower.as_bytes();
    if n.is_empty() {
        return true;
    }
    let h = haystack.as_bytes();
    if h.len() < n.len() {
        return false;
    }
    h.windows(n.len())
        .any(|w| w.iter().zip(n.iter()).all(|(&a, &b)| a.to_ascii_lowercase() == b))
}

impl AppState {
    pub fn sidebar_items_cached(&self, ui: &UiState) -> Arc<[SidebarItem]> {
        let key = SidebarCacheKey {
            pages_gen: self.pages_gen,
            dbs_gen: self.dbs_gen,
            collapsed_gen: ui.collapsed_gen,
            workspace_id: self.workspace_id.clone(),
            pages_len: self.pages.len(),
            pages_ptr: self.pages.as_ptr() as usize,
            dbs_len: self.databases.len(),
            dbs_ptr: self.databases.as_ptr() as usize,
            block_owners_len: self.block_owner_pages.len(),
            child_positions_len: self.child_page_positions.len(),
            collapsed_len: ui.collapsed_nodes.len(),
            collapsed_hash: hash_string_set(&ui.collapsed_nodes),
        };
        {
            let guard = self
                .sidebar_cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(ref cached) = *guard {
                if cached.key == key {
                    return Arc::clone(&cached.items);
                }
            }
        }
        let items: Arc<[SidebarItem]> = Arc::from(self.build_sidebar_items_uncached(ui));
        let mut guard = self
            .sidebar_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = Some(CachedSidebarItems {
            key,
            items: Arc::clone(&items),
        });
        items
    }

    pub fn build_sidebar_items(&self, ui: &UiState) -> Vec<SidebarItem> {
        self.sidebar_items_cached(ui).to_vec()
    }

    fn build_sidebar_items_uncached(&self, ui: &UiState) -> Vec<SidebarItem> {
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

        let mut children_by_parent: HashMap<&str, Vec<&NotionPageSummary>> = HashMap::new();
        let mut roots = Vec::new();
        for p in &active_pages {
            // Never dump database rows as top-level WORKSPACE pages (even if DB wasn't synced yet).
            if p.parent_type == "database_id" {
                continue;
            }
            if !cycle_nodes.contains(p.id.as_str()) {
                if let Some(&pid) = effective_parent.get(p.id.as_str()) {
                    children_by_parent.entry(pid).or_default().push(p);
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
            kids.sort_by_cached_key(|p| {
                let p = *p;
                let pos = self
                    .child_page_positions
                    .get(&p.id)
                    .copied()
                    .unwrap_or(i64::MAX);
                (
                    pos,
                    is_untitled(p),
                    natural_sort_key(p.display_title()),
                    p.id.as_str(),
                )
            });
        }

        roots.sort_by_cached_key(|p| {
            let p = *p;
            (
                is_untitled(p),
                !p.is_favorite,
                natural_sort_key(p.display_title()),
                p.id.as_str(),
            )
        });

        fn push_tree<'a>(
            node: &'a NotionPageSummary,
            depth: usize,
            children_map: &HashMap<&'a str, Vec<&'a NotionPageSummary>>,
            collapsed: &HashSet<String>,
            visited: &mut HashSet<&'a str>,
            out: &mut Vec<SidebarItem>,
        ) {
            if !visited.insert(node.id.as_str()) {
                return;
            }
            let has_kids = children_map
                .get(node.id.as_str())
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
                if let Some(kids) = children_map.get(node.id.as_str()) {
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

    /// Returns borrowed filtered & sorted rows for the active Database without cloning every `NotionPage`.
    pub fn filtered_sorted_db_row_refs(&self, ui: &UiState) -> Vec<&NotionPage> {
        let q = ui.db_filter.trim().to_ascii_lowercase();
        let mut rows: Vec<&NotionPage> = self
            .active_database_rows
            .iter()
            .filter(|r| !r.archived)
            .filter(|r| {
                if q.is_empty() {
                    true
                } else {
                    ascii_contains_ignore_case(&r.title, &q)
                        || ascii_contains_ignore_case(&r.properties_summary(), &q)
                }
            })
            .collect();

        match ui.db_sort {
            DatabaseSortMode::LastEditedDesc => {
                rows.sort_by(|a, b| b.last_edited_time.cmp(&a.last_edited_time));
            }
            DatabaseSortMode::LastEditedAsc => {
                rows.sort_by(|a, b| a.last_edited_time.cmp(&b.last_edited_time));
            }
            DatabaseSortMode::TitleAsc => {
                rows.sort_by_cached_key(|a| a.title.to_ascii_lowercase());
            }
            DatabaseSortMode::TitleDesc => {
                rows.sort_by_cached_key(|a| std::cmp::Reverse(a.title.to_ascii_lowercase()));
            }
            DatabaseSortMode::StatusAsc => {
                let status_key = |p: &NotionPage| -> String {
                    p.properties
                        .values()
                        .find_map(|v| match v {
                            NotionPropertyValue::Status(Some(o))
                            | NotionPropertyValue::Select(Some(o)) => {
                                Some(o.name.to_ascii_lowercase())
                            }
                            _ => None,
                        })
                        .unwrap_or_else(|| "zzz".to_string())
                };
                rows.sort_by_cached_key(|a| status_key(a));
            }
        }
        rows
    }

    /// Returns filtered & sorted rows for the active Database.
    pub fn filtered_sorted_db_rows(&self, ui: &UiState) -> Vec<NotionPage> {
        self.filtered_sorted_db_row_refs(ui)
            .into_iter()
            .cloned()
            .collect()
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

pub(super) fn flatten_visible_blocks(
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
pub(super) fn attach_children_in_tree(
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

pub(super) fn set_todo_in_tree(blocks: &mut [NotionBlock], target_id: &str, checked: bool) -> bool {
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

pub(super) fn all_blocks_have_server_ids(blocks: &[NotionBlock]) -> bool {
    blocks.iter().all(|b| {
        !b.id.trim().is_empty()
            && !b.id.starts_with("__optimistic_")
            && all_blocks_have_server_ids(&b.children)
    })
}

pub(super) fn update_block_in_tree(
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

pub(super) fn find_parent_block_id_in_tree(blocks: &[NotionBlock], target_id: &str) -> Option<String> {
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

pub(super) fn insert_sibling_after_in_tree(
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
pub(super) fn reconcile_server_block_ids(
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

pub(super) fn update_block_media_url_in_tree(blocks: &mut [NotionBlock], target_id: &str, url: &str) -> bool {
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

