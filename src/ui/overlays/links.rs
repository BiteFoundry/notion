//! Link Extractor Overlay (`u`): extracts all URLs (rich-text links, bookmarks, embeds,
//! link previews, bare URLs, and Notion page URL) from the active Notion page.

use std::collections::HashMap;

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::{
    models::{spans_plain_text, NotionBlock, NotionBlockKind, NotionPageDetail, RichTextSpan},
    ui::{
        action::Action,
        app::UiState,
        hitmap::{HitMap, HitTarget},
        overlays::{centered_rect, scroll_selection_clamped, OverlayOutcome},
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedLink {
    pub label: String,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct LinkExtractorOverlay {
    pub context_title: String,
    pub links: Vec<ExtractedLink>,
    pub selected: usize,
}

impl LinkExtractorOverlay {
    pub fn from_notion_page(detail: &NotionPageDetail) -> Self {
        let links = extract_links_from_notion_page(detail);
        Self {
            context_title: detail.page.display_title().to_string(),
            links,
            selected: 0,
        }
    }

    pub fn current_url(&self) -> Option<&str> {
        let idx = self.selected.min(self.links.len().saturating_sub(1));
        self.links.get(idx).map(|l| l.url.as_str())
    }

    pub fn select(&mut self, index: usize) {
        if index < self.links.len() {
            self.selected = index;
        }
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::CloseOverlay
            | Action::OpenLinkExtractor
            | Action::Input('q')
            | Action::Input('u') => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            Action::MoveUp | Action::CursorUp | Action::Input('k') => {
                if !self.links.is_empty() {
                    let cur = self.selected.min(self.links.len() - 1);
                    self.selected = (cur + self.links.len() - 1) % self.links.len();
                }
                OverlayOutcome::Handled
            }
            Action::MoveDown | Action::CursorDown | Action::Input('j') => {
                if !self.links.is_empty() {
                    let cur = self.selected.min(self.links.len() - 1);
                    self.selected = (cur + 1) % self.links.len();
                }
                OverlayOutcome::Handled
            }
            Action::PageUp => {
                self.selected = scroll_selection_clamped(self.selected, self.links.len(), -5);
                OverlayOutcome::Handled
            }
            Action::PageDown => {
                self.selected = scroll_selection_clamped(self.selected, self.links.len(), 5);
                OverlayOutcome::Handled
            }
            Action::CursorHome | Action::ScrollTop | Action::Input('g') => {
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::CursorEnd | Action::ScrollBottom | Action::Input('G') => {
                self.selected = self.links.len().saturating_sub(1);
                OverlayOutcome::Handled
            }
            Action::MouseScroll { delta, .. } => {
                self.selected =
                    scroll_selection_clamped(self.selected, self.links.len(), *delta as isize);
                OverlayOutcome::Handled
            }
            Action::Activate | Action::OverlaySubmit => {
                if let Some(url) = self.current_url() {
                    OverlayOutcome::CloseAndDispatch(Action::OpenUrl(url.to_string()))
                } else {
                    OverlayOutcome::Close
                }
            }
            Action::Input('y') => {
                if let Some(url) = self.current_url() {
                    OverlayOutcome::Dispatch(Action::CopyUrlToClipboard(url.to_string()))
                } else {
                    OverlayOutcome::Handled
                }
            }
            Action::Input(ch) if ch.is_ascii_digit() && *ch != '0' => {
                let idx = (*ch as usize) - ('1' as usize);
                if idx < self.links.len() {
                    self.selected = idx;
                }
                OverlayOutcome::Handled
            }
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
        let theme = &ui.theme;
        let max_w = area.width.saturating_sub(6).clamp(36, 96);
        let max_h = area.height.saturating_sub(4).clamp(9, 22);
        let rect = centered_rect(area, max_w, max_h);
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let title = format!(
            " Links ({}) · {} · Enter Open · y Copy · Esc Close ",
            self.links.len(),
            crate::ui::truncate_width(&self.context_title, 28)
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                crate::ui::truncate_width(&title, rect.width.saturating_sub(2) as usize),
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        if self.links.is_empty() {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "No links found on this page.",
                    theme.muted_style(),
                )),
                inner,
            );
            return;
        }

        let visible_rows = inner.height.saturating_sub(1) as usize;
        let selected = self.selected.min(self.links.len() - 1);
        let offset = crate::ui::nav_pane::selection_offset(selected, visible_rows, self.links.len());

        for (i, link) in self.links.iter().skip(offset).take(visible_rows).enumerate() {
            let idx = offset + i;
            let row_y = inner.y + i as u16;
            let is_sel = idx == selected;
            let row_style = if is_sel {
                theme.selection()
            } else {
                Style::default().bg(theme.bg_elevated).fg(theme.fg)
            };
            let prefix = format!("{:>2}. ", idx + 1);
            let label = crate::ui::truncate_width(&link.label, 30);
            let avail_url = (inner.width as usize).saturating_sub(prefix.len() + label.len() + 4);
            let url_disp = crate::ui::truncate_width(&link.url, avail_url);
            let line = Line::from(vec![
                Span::styled(prefix, row_style.add_modifier(Modifier::BOLD)),
                Span::styled(label, row_style),
                Span::styled("  ", row_style),
                Span::styled(
                    url_disp,
                    if is_sel {
                        row_style.add_modifier(Modifier::UNDERLINED)
                    } else {
                        Style::default().bg(theme.bg_elevated).fg(theme.accent_soft)
                    },
                ),
            ]);
            frame.render_widget(
                Paragraph::new(line).style(row_style),
                Rect::new(inner.x, row_y, inner.width, 1),
            );
            hitmap.add_line(inner.x, row_y, inner.width, HitTarget::OverlayItem { index: idx });
        }

        let footer_y = inner.y + inner.height.saturating_sub(1);
        frame.render_widget(
            Paragraph::new(Span::styled(
                "j/k move · 1-9 jump · Enter open in browser · y copy URL · Esc close",
                theme.muted_style(),
            )),
            Rect::new(inner.x, footer_y, inner.width, 1),
        );
    }
}

pub fn extract_links_from_notion_page(detail: &NotionPageDetail) -> Vec<ExtractedLink> {
    let mut out = Vec::new();
    let mut index_by_url = HashMap::new();
    collect_block_links(&detail.blocks, &mut out, &mut index_by_url);
    for url in find_bare_urls(&detail.page.plain_text) {
        push_link(&mut out, &mut index_by_url, "", &url);
    }
    if !detail.page.url.trim().is_empty() {
        push_link(&mut out, &mut index_by_url, "Open page in Notion Web", &detail.page.url);
    }
    out
}

fn push_rich_text_links(
    spans: &[RichTextSpan],
    out: &mut Vec<ExtractedLink>,
    index_by_url: &mut HashMap<String, usize>,
) {
    for sp in spans {
        if let Some(ref href) = sp.href {
            push_link(out, index_by_url, &sp.plain_text, href);
        }
    }
}

fn collect_block_links(
    blocks: &[NotionBlock],
    out: &mut Vec<ExtractedLink>,
    index_by_url: &mut HashMap<String, usize>,
) {
    for blk in blocks {
        match &blk.kind {
            NotionBlockKind::Paragraph { rich_text, .. }
            | NotionBlockKind::Heading1 { rich_text, .. }
            | NotionBlockKind::Heading2 { rich_text, .. }
            | NotionBlockKind::Heading3 { rich_text, .. }
            | NotionBlockKind::BulletedListItem { rich_text, .. }
            | NotionBlockKind::NumberedListItem { rich_text, .. }
            | NotionBlockKind::ToDo { rich_text, .. }
            | NotionBlockKind::Toggle { rich_text, .. }
            | NotionBlockKind::Callout { rich_text, .. }
            | NotionBlockKind::Quote { rich_text, .. } => {
                push_rich_text_links(rich_text, out, index_by_url);
            }
            NotionBlockKind::TableRow { cells } => {
                for c in cells {
                    push_rich_text_links(c, out, index_by_url);
                }
            }
            NotionBlockKind::Bookmark { url, caption }
            | NotionBlockKind::Embed { url, caption }
            | NotionBlockKind::Pdf { url, caption }
            | NotionBlockKind::Image { url, caption, .. } => {
                push_link(out, index_by_url, &spans_plain_text(caption), url);
            }
            NotionBlockKind::File { url, name, caption } => {
                let lbl = name
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| spans_plain_text(caption));
                push_link(out, index_by_url, &lbl, url);
            }
            NotionBlockKind::LinkPreview { url } => {
                push_link(out, index_by_url, "", url);
            }
            _ => {}
        }
        collect_block_links(&blk.children, out, index_by_url);
    }
}

fn push_link(
    out: &mut Vec<ExtractedLink>,
    index_by_url: &mut HashMap<String, usize>,
    raw_label: &str,
    raw_url: &str,
) {
    let url = clean_url(raw_url);
    if !is_openable_url(&url) {
        return;
    }
    let clean_label = raw_label
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let key = url.to_ascii_lowercase();
    if let Some(&existing_idx) = index_by_url.get(&key) {
        if out[existing_idx].label == out[existing_idx].url && !clean_label.is_empty() {
            out[existing_idx].label = clean_label;
        }
        return;
    }
    let label = if clean_label.is_empty() {
        url.clone()
    } else {
        clean_label
    };
    let idx = out.len();
    out.push(ExtractedLink { label, url });
    index_by_url.insert(key, idx);
}

fn clean_url(raw: &str) -> String {
    raw.trim()
        .trim_end_matches(['.', ',', ';', ')', ']', '>', '"', '\''])
        .to_string()
}

fn is_openable_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("mailto:"))
        && url.len() > 8
}

fn find_bare_urls(text: &str) -> Vec<String> {
    let mut urls = Vec::new();
    for token in text.split_whitespace() {
        let trimmed = token.trim_matches(|c| matches!(c, '(' | '<' | '[' | '"' | '\''));
        if trimmed.starts_with("https://") || trimmed.starts_with("http://") {
            let cleaned = clean_url(trimmed);
            if is_openable_url(&cleaned) {
                urls.push(cleaned);
            }
        }
    }
    urls
}
