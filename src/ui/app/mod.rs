//! Notion TUI application state (`AppState`), UI presentation state (`UiState`),
//! sidebar tree builder, visible block flattener (with live toggle expansion),
//! and database Table / Board (Kanban) / List query helpers.

mod editor_ops;
mod editor_state;
mod events;
mod messages;
mod state;
mod views;

#[cfg(test)]
mod tests;

pub use self::{
    editor_state::{
        apply_text_to_block_kind, block_editable_text, block_type_for_kind,
        is_block_enter_editable, is_block_inline_editable, text_to_rich_spans, BlockCursorInit,
        CanvasBlockLayout, CanvasLinkHit, InlineBlockEditor, InlinePageEditor, InlineSyncStatus,
    },
    state::{
        AppState, BoardColumn, CanvasView, Focus, NarrowPane, NavTarget, PendingPixelGraphic,
        SidebarItem, UiMsg, UiState, VisibleBlock, SIDEBAR_MAX_TREE_DEPTH,
    },
    views::{build_created_page_preview, build_optimistic_append_blocks},
};
