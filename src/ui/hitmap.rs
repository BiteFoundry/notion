//! Rendered screen region -> clickable target mapping (`HitMap`).
//!
//! Rebuilt every frame; later registrations (drawn on top) win on overlap.

use ratatui::layout::{Position, Rect};

use crate::models::DatabaseViewMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonId {
    WorkspaceSwitcher,
    ToggleWidth,
    SyncNow,
    Help,
    Settings,
    Search,
    CommandPalette,
    ToggleSidebar,
    ToggleFavorite,
    ToggleComments,
    QuickAppend,
    NewChildPage,
    EditPage,
    EditExternal,
    OpenWeb,
    LinkExtractor,
    ToggleHrefs,
    ImagePreview,
    Attachments,
    DbFilter,
    DbSort,
    DbNewItem,
    NarrowSidebar,
    NarrowCanvas,
    ConfirmOk,
    ConfirmCancel,
    HtmlToggleMode,
    HtmlServeBrowser,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitTarget {
    Pane(crate::ui::app::Focus),
    SidebarRow(usize),
    SidebarToggle(usize),
    BreadcrumbSegment(usize),
    CanvasBlock(usize),
    TodoToggle(usize),
    ToggleBlock(usize),
    ChildCard(usize),
    MediaChip(usize),
    NotionButton(usize),
    CodeHtmlPreview(usize),
    DbViewTab(DatabaseViewMode),
    DbRow(usize),
    DbBoardCard { col: usize, row: usize },
    Button(ButtonId),
    OverlayBody,
    OverlayItem { index: usize },
}

#[derive(Debug, Clone, Default)]
pub struct HitMap {
    regions: Vec<(Rect, HitTarget)>,
}

impl HitMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.regions.clear();
    }

    pub fn add(&mut self, area: Rect, target: HitTarget) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        self.regions.push((area, target));
    }

    pub fn add_line(&mut self, x: u16, row: u16, width: u16, target: HitTarget) {
        self.add(Rect::new(x, row, width, 1), target);
    }

    pub fn hit(&self, row: u16, col: u16) -> Option<HitTarget> {
        let pos = Position::new(col, row);
        self.regions
            .iter()
            .rev()
            .find(|(area, _)| area.contains(pos))
            .map(|(_, target)| *target)
    }

    pub fn region_of(&self, target: HitTarget) -> Option<Rect> {
        self.regions
            .iter()
            .rev()
            .find(|(_, item)| *item == target)
            .map(|(area, _)| *area)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hitmap_resolves_topmost_overlapping_target() {
        let mut map = HitMap::new();
        map.add(Rect::new(0, 0, 40, 10), HitTarget::CanvasBlock(0));
        map.add_line(2, 2, 6, HitTarget::TodoToggle(0));
        assert_eq!(map.hit(2, 3), Some(HitTarget::TodoToggle(0)));
        assert_eq!(map.hit(0, 0), Some(HitTarget::CanvasBlock(0)));
        assert_eq!(map.hit(15, 15), None);
    }
}
