use std::{borrow::Cow, collections::HashSet, ops::Range, path::PathBuf};

use anyhow::{bail, ensure, Result};
use serde_json::{json, Value};

use crate::{
    models::{normalize_notion_id, NotionBlock, NotionBlockKind, NotionPageDetail, RichTextKind, RichTextSpan},
    notion::NotionClient,
    parser::{
        blocks_to_editor_markdown, has_column_blocks, is_markdown_table_separator,
        is_preserved_anchor_block, markdown_to_notion_blocks,
        notion::{
            block_inline_markdown, inline_markdown_spans, markdown_edit_ranges_with_options,
            rich_text_to_markdown,
        },
        parse_inline_markdown_rich_text, parse_rich_text_array,
    },
};

#[derive(Default)]
pub(crate) struct PageEdit {
    inserts: Vec<Insertion>,
    patches: Vec<BlockPatch>,
    archives: Vec<String>,
}

struct BlockPatch {
    id: String,
    kind: String,
    fields: serde_json::Map<String, Value>,
    text: Option<(Vec<RichTextSpan>, String)>,
}

struct Insertion {
    parent: String,
    after: Option<String>,
    at_start: bool,
    children: Vec<Value>,
}

struct Draft {
    source: String,
    payload: Value,
    children_markdown: String,
}

impl Draft {
    fn kind(&self) -> &str {
        self.payload["type"].as_str().expect("Markdown block type")
    }

    fn into_payload(mut self) -> Result<Value> {
        let kind = self.kind().to_owned();
        let child_lines: Vec<&str> = self.children_markdown.lines().collect();
        let children = parse_drafts(&child_lines)?;
        if !children.is_empty() {
            let mut child_payloads = Vec::with_capacity(children.len());
            for child in children {
                child_payloads.push(child.into_payload()?);
            }
            self.payload[&kind]["children"] = Value::Array(child_payloads);
        }
        Ok(self.payload)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ColumnMarker<'a> {
    ColumnListStart(&'a str),
    ColumnStart(&'a str),
    ColumnListEnd,
    Malformed,
}

struct ParsedColumnList {
    id: String,
    columns: Vec<(String, String)>,
}

fn classify_column_marker(line: &str) -> Option<ColumnMarker<'_>> {
    let trimmed = line.trim();
    let after_open = trimmed.strip_prefix("<!--")?.trim_start();
    if !after_open.starts_with("notion:column") && !after_open.starts_with("/notion:column") {
        return None;
    }
    let Some(inner) = after_open.strip_suffix("-->") else {
        return Some(ColumnMarker::Malformed);
    };
    let inner = inner.trim();
    if inner == "/notion:column-list" {
        return Some(ColumnMarker::ColumnListEnd);
    }
    if let Some(rest) = inner.strip_prefix("notion:column-list") {
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            if let Some(id) = rest.trim().strip_prefix("id=") {
                let id = id.trim();
                if !id.is_empty() && !id.chars().any(char::is_whitespace) {
                    return Some(ColumnMarker::ColumnListStart(id));
                }
            }
        }
        return Some(ColumnMarker::Malformed);
    }
    if let Some(rest) = inner.strip_prefix("notion:column") {
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            if let Some(id) = rest.trim().strip_prefix("id=") {
                let id = id.trim();
                if !id.is_empty() && !id.chars().any(char::is_whitespace) {
                    return Some(ColumnMarker::ColumnStart(id));
                }
            }
        }
        return Some(ColumnMarker::Malformed);
    }
    Some(ColumnMarker::Malformed)
}

fn classify_direct_column_marker(line: &str) -> Option<ColumnMarker<'_>> {
    if line.starts_with("  ") || line.starts_with('\t') {
        return None;
    }
    classify_column_marker(line)
}

fn contains_direct_column_marker(markdown: &str) -> bool {
    let mut fenced = false;
    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if !fenced && classify_direct_column_marker(line).is_some() {
            return true;
        }
    }
    false
}

pub(crate) fn contains_column_marker(markdown: &str) -> bool {
    let mut fenced = false;
    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if !fenced && classify_column_marker(line).is_some() {
            return true;
        }
    }
    false
}

pub(crate) fn editor_markdown_for_detail(detail: &NotionPageDetail) -> String {
    if has_column_blocks(&detail.blocks) {
        blocks_to_editor_markdown(&detail.blocks)
    } else {
        detail.markdown.clone()
    }
}

pub(crate) fn save_edit_recovery(page_id: &str, edited: &str) -> Result<PathBuf> {
    let safe_id: String = page_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let file_name = format!(
        "page-edit-{}-{}-{}.md",
        if safe_id.is_empty() { "page" } else { &safe_id },
        std::process::id(),
        ts
    );
    let preferred = crate::config::data_dir().join("recovery").join(&file_name);
    if crate::config::ensure_parent(&preferred).is_ok()
        && std::fs::write(&preferred, edited.as_bytes()).is_ok()
    {
        return Ok(preferred);
    }
    let fallback = std::env::temp_dir().join(file_name);
    std::fs::write(&fallback, edited.as_bytes())?;
    Ok(fallback)
}

fn split_column_lists(markdown: &str) -> Result<(Vec<String>, Vec<ParsedColumnList>)> {
    let mut segments = Vec::new();
    let mut column_lists = Vec::new();
    let mut cur_segment: Vec<&str> = Vec::new();
    let mut active_list_id = String::new();
    let mut active_columns: Vec<(String, String)> = Vec::new();
    let mut cur_col: Option<(String, Vec<&str>)> = None;
    let mut depth = 0usize;
    let mut fenced = false;

    for line in markdown.lines() {
        let is_fence = line.trim_start().starts_with("```");
        if is_fence {
            fenced = !fenced;
            if depth == 0 {
                cur_segment.push(line);
            } else if let Some((_, ref mut col_lines)) = cur_col {
                col_lines.push(line);
            } else {
                bail!(
                    "Content inside column list {} must appear under a column marker",
                    active_list_id
                );
            }
            continue;
        }
        let marker = if fenced {
            None
        } else {
            classify_direct_column_marker(line)
        };
        match (depth, marker) {
            (0, None) => cur_segment.push(line),
            (0, Some(ColumnMarker::ColumnListStart(id))) => {
                segments.push(cur_segment.join("\n"));
                cur_segment.clear();
                active_list_id = id.to_owned();
                active_columns.clear();
                cur_col = None;
                depth = 1;
            }
            (0, Some(ColumnMarker::ColumnStart(id))) => {
                bail!(
                    "Column marker for {} appeared outside a column list",
                    id
                );
            }
            (0, Some(ColumnMarker::ColumnListEnd)) => {
                bail!("Closing column-list marker appeared without a matching start marker");
            }
            (_, Some(ColumnMarker::Malformed)) => {
                bail!("Malformed column layout marker: {}", line.trim());
            }
            (1, Some(ColumnMarker::ColumnStart(id))) => {
                if let Some((prev_id, prev_lines)) = cur_col.take() {
                    active_columns.push((prev_id, prev_lines.join("\n")));
                }
                cur_col = Some((id.to_owned(), Vec::new()));
            }
            (1, Some(ColumnMarker::ColumnListEnd)) => {
                if let Some((prev_id, prev_lines)) = cur_col.take() {
                    active_columns.push((prev_id, prev_lines.join("\n")));
                }
                column_lists.push(ParsedColumnList {
                    id: std::mem::take(&mut active_list_id),
                    columns: std::mem::take(&mut active_columns),
                });
                depth = 0;
            }
            (1, Some(ColumnMarker::ColumnListStart(_))) => {
                let Some((_, ref mut col_lines)) = cur_col else {
                    bail!(
                        "Nested column list inside {} must appear under a column marker",
                        active_list_id
                    );
                };
                col_lines.push(line);
                depth += 1;
            }
            (1, None) => {
                if let Some((_, ref mut col_lines)) = cur_col {
                    col_lines.push(line);
                } else if !line.trim().is_empty() {
                    bail!(
                        "Content inside column list {} must appear under a column marker",
                        active_list_id
                    );
                }
            }
            (d, Some(ColumnMarker::ColumnListStart(_))) => {
                if let Some((_, ref mut col_lines)) = cur_col {
                    col_lines.push(line);
                }
                depth = d + 1;
            }
            (d, Some(ColumnMarker::ColumnListEnd)) => {
                if let Some((_, ref mut col_lines)) = cur_col {
                    col_lines.push(line);
                }
                depth = d - 1;
            }
            (_, _) => {
                if let Some((_, ref mut col_lines)) = cur_col {
                    col_lines.push(line);
                }
            }
        }
    }

    ensure!(
        depth == 0,
        "Unclosed column list marker for {}",
        active_list_id
    );
    segments.push(cur_segment.join("\n"));
    Ok((segments, column_lists))
}

impl PageEdit {
    pub(crate) fn plan(page_id: &str, existing: &[NotionBlock], markdown: &str) -> Result<Self> {
        let mut plan = Self::default();
        plan.siblings(page_id, existing, markdown)?;
        Ok(plan)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.inserts.is_empty() && self.patches.is_empty() && self.archives.is_empty()
    }

    pub(crate) async fn apply(self, client: &NotionClient, page_id: &str) -> Result<()> {
        let mut patches = Vec::with_capacity(self.patches.len());
        for patch in self.patches {
            let mut fields = patch.fields;
            if let Some((previous, inline)) = patch.text {
                let original = if previous
                    .iter()
                    .any(|s| matches!(s.kind, RichTextKind::Mention { .. }))
                {
                    let raw = client.get_block_json(&patch.id).await?;
                    let rich = raw[&patch.kind].get("rich_text");
                    ensure!(
                        parse_rich_text_array(rich) == previous,
                        "Block {} changed since editing began",
                        patch.id
                    );
                    rich.and_then(Value::as_array).cloned().unwrap_or_default()
                } else {
                    previous.iter().map(span_to_json).collect()
                };
                fields.insert(
                    "rich_text".into(),
                    json!(edited_rich_text(&original, &inline)?),
                );
            }
            patches.push((patch.id, json!({ (patch.kind): fields })));
        }
        for insertion in self.inserts {
            client
                .insert_block_children(
                    &insertion.parent,
                    insertion.after.as_deref(),
                    insertion.at_start,
                    insertion.children,
                )
                .await?;
        }
        for (id, payload) in patches {
            client.update_block(&id, page_id, payload).await?;
        }
        // Nothing is archived until every addition (including all batches) and patch succeeded.
        for id in self.archives {
            client.delete_block(&id).await?;
        }
        Ok(())
    }

    fn siblings(&mut self, parent: &str, old: &[NotionBlock], markdown: &str) -> Result<()> {
        self.siblings_with_after(parent, old, markdown, None)
    }

    fn siblings_with_after(
        &mut self,
        parent: &str,
        old: &[NotionBlock],
        markdown: &str,
        initial_after: Option<String>,
    ) -> Result<()> {
        if contains_direct_column_marker(markdown) {
            let (segments, parsed_lists) = split_column_lists(markdown)?;
            let mut old_cls: Vec<&NotionBlock> = Vec::new();
            let mut seg_groups: Vec<Vec<SegmentGroup<'_>>> = vec![Vec::new()];
            let mut seg_fallbacks: Vec<(&str, Option<String>)> = vec![(parent, initial_after.clone())];
            collect_same_depth_columns(
                parent,
                old,
                initial_after,
                &mut old_cls,
                &mut seg_groups,
                &mut seg_fallbacks,
            );
            ensure!(
                !old_cls.is_empty(),
                "Unexpected column layout markers: this container has no column lists"
            );
            ensure!(
                old_cls.len() == parsed_lists.len(),
                "Column layout mismatch: expected {} column list(s), found {}",
                old_cls.len(),
                parsed_lists.len()
            );

            for (idx, (old_cl, parsed_cl)) in old_cls.iter().zip(parsed_lists.iter()).enumerate() {
                ensure!(
                    normalize_notion_id(&old_cl.id) == normalize_notion_id(&parsed_cl.id),
                    "Column list marker id {} does not match existing block {}",
                    parsed_cl.id,
                    old_cl.id
                );
                ensure!(
                    !old_cl.has_children || !old_cl.children.is_empty(),
                    "Columns of block {} were not loaded",
                    old_cl.id
                );
                ensure!(
                    old_cl
                        .children
                        .iter()
                        .all(|c| matches!(c.kind, NotionBlockKind::Column)),
                    "Column list {} contains unexpected non-column child blocks",
                    old_cl.id
                );
                ensure!(
                    old_cl.children.len() == parsed_cl.columns.len(),
                    "Column boundary mismatch in column list {}: expected {} column(s), found {}",
                    old_cl.id,
                    old_cl.children.len(),
                    parsed_cl.columns.len()
                );

                self.apply_segment_groups(&seg_groups[idx], &seg_fallbacks[idx], &segments[idx])?;

                for (old_col, (parsed_col_id, col_md)) in
                    old_cl.children.iter().zip(parsed_cl.columns.iter())
                {
                    ensure!(
                        normalize_notion_id(&old_col.id) == normalize_notion_id(parsed_col_id),
                        "Column marker id {} does not match existing column {} in column list {}",
                        parsed_col_id,
                        old_col.id,
                        old_cl.id
                    );
                    ensure!(
                        !old_col.has_children || !old_col.children.is_empty(),
                        "Children of column {} were not loaded",
                        old_col.id
                    );
                    self.siblings_with_after(&old_col.id, &old_col.children, col_md, None)?;
                }
            }

            return self.apply_segment_groups(
                &seg_groups[old_cls.len()],
                &seg_fallbacks[old_cls.len()],
                &segments[old_cls.len()],
            );
        }

        self.siblings_plain(parent, old, markdown, initial_after)
    }

    fn apply_segment_groups(
        &mut self,
        groups: &[SegmentGroup<'_>],
        fallback: &(&str, Option<String>),
        segment_md: &str,
    ) -> Result<()> {
        match groups {
            [] => self.siblings_plain(fallback.0, &[], segment_md, fallback.1.clone()),
            [only] => self.siblings_plain(only.parent, only.blocks, segment_md, only.after.clone()),
            many => {
                let include_markers = contains_column_marker(segment_md);
                let new_lines: Vec<&str> = segment_md.lines().collect();
                let mut cursor = 0usize;
                for (g_idx, group) in many.iter().enumerate() {
                    if g_idx + 1 == many.len() {
                        let tail = new_lines[cursor..].join("\n");
                        self.siblings_plain(group.parent, group.blocks, &tail, group.after.clone())?;
                        break;
                    }
                    let next_group = &many[g_idx + 1];
                    let (next_rendered, _) =
                        markdown_edit_ranges_with_options(next_group.blocks, include_markers);
                    let next_first_vis = next_rendered
                        .lines()
                        .find(|l| !l.trim().is_empty());
                    let split = next_first_vis
                        .and_then(|needle| {
                            (cursor..new_lines.len()).find(|&pos| new_lines[pos] == needle)
                        })
                        .unwrap_or(new_lines.len());
                    let part = new_lines[cursor..split].join("\n");
                    self.siblings_plain(group.parent, group.blocks, &part, group.after.clone())?;
                    cursor = split;
                }
                Ok(())
            }
        }
    }

    fn siblings_plain(
        &mut self,
        parent: &str,
        old: &[NotionBlock],
        markdown: &str,
        initial_after: Option<String>,
    ) -> Result<()> {
        let include_column_markers = contains_column_marker(markdown);
        let (original, ranges) = markdown_edit_ranges_with_options(old, include_column_markers);
        let old_lines: Vec<_> = original.lines().collect();
        let new_lines: Vec<_> = markdown.lines().collect();
        let old_visible: Vec<_> = visible_lines(&old_lines);
        let new_visible: Vec<_> = visible_lines(&new_lines);
        let mut hosted = HashSet::new();
        hosted_urls(old, &mut hosted);
        let old_display = comparable_lines(&old_lines, &hosted);
        let new_display = comparable_lines(&new_lines, &hosted);
        let matches = alignment(
            &old_visible
                .iter()
                .map(|&i| old_display[i].as_ref())
                .collect::<Vec<_>>(),
            &new_visible
                .iter()
                .map(|&i| new_display[i].as_ref())
                .collect::<Vec<_>>(),
        );

        let mut old_start = 0;
        let mut new_start = 0;
        let mut after = initial_after;
        for (index, range) in ranges.into_iter().enumerate() {
            let start = old_visible.partition_point(|&line| line < range.start);
            let end = old_visible.partition_point(|&line| line < range.end);
            let Some(matched) = matched_range(&matches, start..end) else {
                continue;
            };
            let first = new_visible[matched.start];
            let last = new_visible[matched.end - 1];
            // Compare interior blank lines too (for example in a fenced code block).
            if old_display[old_visible[start]..=old_visible[end - 1]] != new_display[first..=last] {
                continue;
            }
            self.changed_siblings(
                parent,
                &old[old_start..index],
                &new_lines[new_start..first],
                &mut after,
            )?;
            after = Some(old[index].id.clone());
            old_start = index + 1;
            new_start = last + 1;
        }
        self.changed_siblings(
            parent,
            &old[old_start..],
            &new_lines[new_start..],
            &mut after,
        )
    }

    fn changed_siblings(
        &mut self,
        parent: &str,
        old: &[NotionBlock],
        lines: &[&str],
        after: &mut Option<String>,
    ) -> Result<()> {
        if let Some((old_idx, line_range)) = find_unchanged_protected_anchor(old, lines) {
            self.changed_siblings(parent, &old[..old_idx], &lines[..line_range.start], after)?;
            *after = Some(old[old_idx].id.clone());
            return self.changed_siblings(
                parent,
                &old[old_idx + 1..],
                &lines[line_range.end..],
                after,
            );
        }

        let non_empty_old: Vec<(usize, &NotionBlock)> = old
            .iter()
            .enumerate()
            .filter(|(_, block)| !is_empty_paragraph(block))
            .collect();
        if let [(_, block)] = non_empty_old.as_slice() {
            let previous = rich_text(&block.kind);
            let rendered = rich_text_to_markdown(previous);
            if block.block_type == "paragraph"
                && rendered.contains('\n')
                && block.children.is_empty()
            {
                let edited = lines.join("\n");
                let text = edited.trim_matches('\n');
                let same_paragraph = text.lines().count() == rendered.lines().count()
                    && markdown_to_notion_blocks(text)
                        .iter()
                        .all(|value| value["type"] == "paragraph");
                if same_paragraph {
                    if text != rendered {
                        self.patches.push(BlockPatch {
                            id: block.id.clone(),
                            kind: block.block_type.clone(),
                            fields: serde_json::Map::new(),
                            text: Some((previous.to_vec(), text.to_owned())),
                        });
                    }
                    *after = old.last().map(|b| b.id.clone());
                    return Ok(());
                }
            }
        }
        let mut drafts = parse_drafts(lines)?.into_iter().peekable();
        let editable = old.iter().filter(|block| !opaque(block)).count();
        let same_count = editable == drafts.len();
        let last_editable = old.iter().rposition(|block| !opaque(block));
        let mut new_blocks = Vec::new();
        for (index, block) in old.iter().enumerate() {
            if opaque(block) {
                if matches!(
                    block.kind,
                    NotionBlockKind::ColumnList | NotionBlockKind::Column
                ) || (matches!(block.kind, NotionBlockKind::SyncedBlock { .. })
                    && has_column_blocks(&block.children))
                {
                    bail!(
                        "Cannot edit column layout {} without intact column markers. Reopen the page and keep the <!-- notion:column ... --> markers.",
                        block.id,
                    );
                }
                let original = crate::parser::blocks_to_markdown(std::slice::from_ref(block));
                let partly_matched = original
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .any(|line| lines.contains(&line));
                ensure!(
                    !partly_matched,
                    "Cannot partially edit the Markdown display of block {}",
                    block.id,
                );
                // A draft that omits the trailing protected block still places new
                // siblings before it, as it did when that block was visible.
                if last_editable.is_none_or(|last| index > last) {
                    for draft in drafts.by_ref() {
                        new_blocks.push(draft.into_payload()?);
                    }
                }
                self.queue_insertion(parent, after, &mut new_blocks);
                *after = Some(block.id.clone());
                continue;
            }
            if let Some(draft) = drafts.peek() {
                if can_patch_in_place(block, draft) {
                    self.queue_insertion(parent, after, &mut new_blocks);
                    self.patch(block, drafts.next().unwrap())?;
                    *after = Some(block.id.clone());
                    continue;
                }
                // An existing subtree has no lossless replacement in Markdown.
                ensure!(
                    !block.has_children && block.children.is_empty(),
                    "Cannot replace the type of block {} while it has children",
                    block.id,
                );
                if same_count {
                    new_blocks.push(drafts.next().unwrap().into_payload()?);
                }
            }
            // Hidden children (e.g. depth-capped reads) and structural blocks keep their IDs.
            if !block.has_children && block.children.is_empty() {
                self.archives.push(block.id.clone());
            } else {
                *after = Some(block.id.clone());
            }
        }
        for draft in drafts {
            new_blocks.push(draft.into_payload()?);
        }
        self.queue_insertion(parent, after, &mut new_blocks);
        Ok(())
    }

    fn queue_insertion(&mut self, parent: &str, after: &Option<String>, children: &mut Vec<Value>) {
        if !children.is_empty() {
            self.inserts.push(Insertion {
                parent: parent.to_owned(),
                after: after.clone(),
                at_start: after.is_none(),
                children: std::mem::take(children),
            });
        }
    }

    fn patch(&mut self, block: &NotionBlock, draft: Draft) -> Result<()> {
        let kind = block.block_type.as_str();
        let draft_kind = draft.kind();
        let mut fields = serde_json::Map::new();
        let mut text = None;
        let desired = &draft.payload[draft_kind];
        let previous = rich_text(&block.kind);
        if kind == "code" {
            let new_text = parse_rich_text_array(desired.get("rich_text"));
            let new_plain = crate::models::spans_plain_text(&new_text);
            if crate::models::spans_plain_text(previous) != new_plain {
                fields.insert("rich_text".into(), json!(code_rich_text(&new_plain)));
            }
        } else {
            let first_line = draft.source.lines().next().unwrap_or_default();
            let inline = if let NotionBlockKind::Callout { icon, .. } = &block.kind {
                let after_gt = first_line.trim().strip_prefix("> ").unwrap_or(first_line.trim());
                if let Some(ic) = icon.as_deref() {
                    if let Some(rest) = after_gt.strip_prefix(ic) {
                        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
                            rest.trim()
                        } else {
                            block_inline_markdown(kind, first_line)
                        }
                    } else if let Some((_, rest)) = crate::parser::notion::split_leading_emoji(after_gt) {
                        rest.trim()
                    } else {
                        after_gt.trim()
                    }
                } else {
                    block_inline_markdown(kind, first_line)
                }
            } else {
                block_inline_markdown(kind, first_line)
            };
            if rich_text_to_markdown(previous) != inline {
                text = Some((previous.to_vec(), inline.to_owned()));
            }
        }
        match &block.kind {
            NotionBlockKind::ToDo { checked, .. } if desired["checked"] != *checked => {
                fields.insert("checked".into(), desired["checked"].clone());
            }
            NotionBlockKind::Code { language, .. } if desired["language"] != language.as_str() => {
                fields.insert("language".into(), desired["language"].clone());
            }
            NotionBlockKind::Callout { icon, .. }
                if draft_kind == "callout"
                    && desired["icon"]["emoji"].as_str() != icon.as_deref()
                    && icon.as_deref().is_some_and(|s| s != "🖼") =>
            {
                fields.insert("icon".into(), desired["icon"].clone());
            }
            _ => {}
        }
        if !fields.is_empty() || text.is_some() {
            self.patches.push(BlockPatch {
                id: block.id.clone(),
                kind: kind.into(),
                fields,
                text,
            });
        }
        let renders_children = matches!(
            kind,
            "heading_1"
                | "heading_2"
                | "heading_3"
                | "paragraph"
                | "quote"
                | "bulleted_list_item"
                | "numbered_list_item"
                | "to_do"
                | "toggle"
                | "callout"
        );
        let has_draft_children = !draft.children_markdown.trim().is_empty();
        if has_draft_children || (renders_children && !block.children.is_empty()) {
            ensure!(
                !block.has_children || !block.children.is_empty(),
                "Children of block {} were not loaded",
                block.id
            );
            self.siblings(&block.id, &block.children, &draft.children_markdown)?;
        }
        Ok(())
    }
}

struct SegmentGroup<'a> {
    parent: &'a str,
    blocks: &'a [NotionBlock],
    after: Option<String>,
}

fn has_same_depth_column_lists(blocks: &[NotionBlock]) -> bool {
    blocks.iter().any(|b| match &b.kind {
        NotionBlockKind::ColumnList => true,
        NotionBlockKind::SyncedBlock { .. } => has_same_depth_column_lists(&b.children),
        _ => false,
    })
}

fn collect_same_depth_columns<'a>(
    parent: &'a str,
    old: &'a [NotionBlock],
    initial_after: Option<String>,
    slots: &mut Vec<&'a NotionBlock>,
    seg_groups: &mut Vec<Vec<SegmentGroup<'a>>>,
    seg_fallbacks: &mut Vec<(&'a str, Option<String>)>,
) {
    let mut run_start = 0usize;
    let mut run_after = initial_after;

    for (idx, blk) in old.iter().enumerate() {
        let is_direct_cl = matches!(blk.kind, NotionBlockKind::ColumnList);
        let is_synced_with_cl = matches!(blk.kind, NotionBlockKind::SyncedBlock { .. })
            && has_same_depth_column_lists(&blk.children);
        if !is_direct_cl && !is_synced_with_cl {
            continue;
        }
        if run_start < idx {
            seg_groups.last_mut().unwrap().push(SegmentGroup {
                parent,
                blocks: &old[run_start..idx],
                after: run_after.clone(),
            });
        }
        if is_direct_cl {
            slots.push(blk);
            seg_groups.push(Vec::new());
            run_after = Some(blk.id.clone());
            seg_fallbacks.push((parent, run_after.clone()));
            run_start = idx + 1;
        } else {
            collect_same_depth_columns(
                &blk.id,
                &blk.children,
                None,
                slots,
                seg_groups,
                seg_fallbacks,
            );
            run_after = Some(blk.id.clone());
            // Outer container owns insertions after the synced_block itself.
            if let Some(last_fb) = seg_fallbacks.last_mut() {
                *last_fb = (parent, run_after.clone());
            }
            run_start = idx + 1;
        }
    }

    if run_start < old.len() {
        seg_groups.last_mut().unwrap().push(SegmentGroup {
            parent,
            blocks: &old[run_start..],
            after: run_after,
        });
    }
}

fn visible_lines(lines: &[&str]) -> Vec<usize> {
    lines
        .iter()
        .enumerate()
        .filter_map(|(i, line)| (!line.trim().is_empty()).then_some(i))
        .collect()
}

fn hosted_urls<'a>(blocks: &'a [NotionBlock], urls: &mut HashSet<&'a str>) {
    for block in blocks {
        match &block.kind {
            NotionBlockKind::Image {
                url,
                is_hosted: true,
                ..
            }
            | NotionBlockKind::Audio {
                url,
                is_hosted: true,
                ..
            }
            | NotionBlockKind::Video {
                url,
                is_hosted: true,
                ..
            }
            | NotionBlockKind::File { url, .. }
            | NotionBlockKind::Pdf { url, .. } => {
                urls.insert(asset_path(url));
            }
            _ => {}
        }
        hosted_urls(&block.children, urls);
    }
}

fn asset_path(url: &str) -> &str {
    url.split(['?', '#']).next().unwrap_or(url)
}

// Generated numbering and expiring URL signatures do not identify a different block.
fn comparable_lines<'a>(lines: &[&'a str], hosted: &HashSet<&str>) -> Vec<Cow<'a, str>> {
    let mut fenced = false;
    lines
        .iter()
        .map(|&line| {
            let trimmed = line.trim_start();
            if trimmed.starts_with("```") {
                fenced = !fenced;
            } else if !fenced {
                let indent = &line[..line.len() - trimmed.len()];
                if let Some(body) = crate::parser::notion::strip_numbered_prefix(trimmed) {
                    return Cow::Owned(format!("{indent}1. {body}"));
                }
                if let Some((label, url)) = line.strip_suffix(')').and_then(|s| s.rsplit_once("]("))
                {
                    let path = asset_path(url);
                    if path != url && hosted.contains(path) {
                        return Cow::Owned(format!("{label}]({path})"));
                    }
                }
            }
            Cow::Borrowed(line)
        })
        .collect()
}

fn opaque(block: &NotionBlock) -> bool {
    is_preserved_anchor_block(block)
        || (!patchable(&block.block_type)
            && !matches!(block.block_type.as_str(), "divider" | "image"))
        || (block.has_children && block.children.is_empty())
        || (block.block_type == "paragraph" && rich_text(&block.kind).is_empty())
}

pub(crate) fn same_edit_baseline(original: &[NotionBlock], current: &[NotionBlock]) -> bool {
    original.len() == current.len()
        && original.iter().zip(current).all(|(a, b)| {
            a.id == b.id
                && a.block_type == b.block_type
                && a.has_children == b.has_children
                && a.archived == b.archived
                && ((opaque(a) && opaque(b)) || a.kind == b.kind)
                && same_edit_baseline(&a.children, &b.children)
        })
}

/// Whether re-applying `edited_markdown` to `fresh` after a stale-ID failure keeps everyone
/// else's changes. Every block in `fresh` must either be unchanged from `baseline`, have its
/// text already in the edit (one of our own writes that landed before the failure), or be a
/// cached block that came back under a new ID with the same content. Blocks that exist only in
/// `baseline` were deleted remotely, which caused the failure, and are tolerated.
pub(crate) fn retry_is_safe(baseline: &[NotionBlock], fresh: &[NotionBlock], edited_markdown: &str) -> bool {
    fn index<'a>(blocks: &'a [NotionBlock], out: &mut std::collections::HashMap<&'a str, &'a NotionBlock>) {
        for block in blocks {
            out.insert(block.id.as_str(), block);
            index(&block.children, out);
        }
    }
    let mut known = std::collections::HashMap::new();
    index(baseline, &mut known);
    let mut fresh_ids = std::collections::HashMap::new();
    index(fresh, &mut fresh_ids);
    // Cached blocks whose ID is gone; each may account for one fresh block with the same content.
    let mut gone: Vec<Option<&NotionBlock>> = known
        .values()
        .filter(|b| !fresh_ids.contains_key(b.id.as_str()))
        .map(|b| Some(*b))
        .collect();
    let same_content = |a: &NotionBlock, b: &NotionBlock| {
        a.block_type == b.block_type
            && a.archived == b.archived
            && ((opaque(a) && opaque(b)) || a.kind == b.kind)
    };
    let edited_lines: HashSet<&str> = edited_markdown.lines().map(str::trim).collect();
    let in_edit = |block: &NotionBlock| {
        let mut own = block.clone();
        own.children.clear();
        own.has_children = false;
        crate::parser::blocks_to_markdown(std::slice::from_ref(&own))
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .all(|line| edited_lines.contains(line))
    };
    fn walk(blocks: &[NotionBlock], ok: &mut dyn FnMut(&NotionBlock) -> bool) -> bool {
        blocks.iter().all(|b| ok(b) && walk(&b.children, ok))
    }
    walk(fresh, &mut |block| match known.get(block.id.as_str()) {
        Some(before) => same_content(before, block) || in_edit(block),
        None => {
            if let Some(slot) = gone.iter_mut().find(|g| g.is_some_and(|g| same_content(g, block))) {
                *slot = None;
                return true;
            }
            in_edit(block)
        }
    })
}

fn can_patch_in_place(block: &NotionBlock, draft: &Draft) -> bool {
    if !patchable(&block.block_type) {
        return false;
    }
    if draft.kind() == block.block_type {
        return true;
    }
    let first_line = draft.source.lines().next().unwrap_or_default().trim();
    if first_line.starts_with("> ") && !first_line.starts_with("> ▸ ") {
        return matches!(
            (block.block_type.as_str(), draft.kind()),
            ("quote", "callout") | ("callout", "quote")
        );
    }
    false
}

pub(crate) fn patchable(kind: &str) -> bool {
    matches!(
        kind,
        "paragraph"
            | "heading_1"
            | "heading_2"
            | "heading_3"
            | "bulleted_list_item"
            | "numbered_list_item"
            | "to_do"
            | "toggle"
            | "callout"
            | "quote"
            | "code"
    )
}

fn is_empty_paragraph(block: &NotionBlock) -> bool {
    block.block_type == "paragraph"
        && rich_text(&block.kind).is_empty()
        && !block.has_children
        && block.children.is_empty()
}

fn is_protected_display_line(line: &str) -> bool {
    [
        "[unsupported:",
        "[📎 ",
        "[📄 ",
        "[🎬 ",
        "[🎵 ",
        "📄 **",
        "🗂 **",
        "[Table of Contents]",
        "[⚡ ",
    ]
    .iter()
    .any(|prefix| line.trim_start().starts_with(prefix))
}

fn find_unchanged_protected_anchor(
    old: &[NotionBlock],
    lines: &[&str],
) -> Option<(usize, Range<usize>)> {
    if old.is_empty() || lines.is_empty() {
        return None;
    }
    let mut hosted = HashSet::new();
    hosted_urls(old, &mut hosted);
    let new_display = comparable_lines(lines, &hosted);
    for (idx, block) in old.iter().enumerate() {
        if !opaque(block)
            || is_empty_paragraph(block)
            || matches!(
                block.kind,
                NotionBlockKind::ColumnList | NotionBlockKind::Column
            )
        {
            continue;
        }
        let rendered = crate::parser::blocks_to_markdown(std::slice::from_ref(block));
        let rendered_lines: Vec<&str> = rendered.lines().collect();
        let vis = visible_lines(&rendered_lines);
        if vis.is_empty() {
            continue;
        }
        let needle_raw = &rendered_lines[vis[0]..=*vis.last().unwrap()];
        let needle = comparable_lines(needle_raw, &hosted);
        if needle.len() > new_display.len() {
            continue;
        }
        if let Some(pos) = (0..=new_display.len() - needle.len())
            .find(|&p| new_display[p..p + needle.len()] == needle[..])
        {
            return Some((idx, pos..pos + needle.len()));
        }
    }
    None
}

fn parse_drafts(lines: &[&str]) -> Result<Vec<Draft>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.trim().is_empty() {
            i += 1;
            continue;
        }
        ensure!(
            !line.starts_with("  "),
            "Indented Markdown has no editable parent"
        );
        ensure!(
            classify_column_marker(line).is_none(),
            "Column layout markers cannot be inserted as plain Markdown blocks"
        );
        ensure!(
            !is_protected_display_line(line),
            "Cannot edit the Markdown display of a protected block",
        );
        let start = i;
        i += 1;
        if line.trim().starts_with("```") {
            while i < lines.len() {
                let end = lines[i].trim().starts_with("```");
                i += 1;
                if end {
                    break;
                }
            }
        }
        // A GFM table (header, separator, rows) is one `table` block, not a paragraph per line.
        let is_table = line.trim().starts_with('|')
            && lines.get(i).is_some_and(|next| is_markdown_table_separator(next));
        if is_table {
            i += 1;
            while i < lines.len() && lines[i].trim().starts_with('|') {
                i += 1;
            }
        }
        let head = lines[start..i].join("\n");
        let mut payloads = markdown_to_notion_blocks(&head);
        ensure!(payloads.len() == 1, "Cannot parse an edited Markdown block");
        let child_start = i;
        // A table's rows are its children; it takes no indented child blocks.
        while !is_table && i < lines.len() && (lines[i].trim().is_empty() || lines[i].starts_with("  ")) {
            i += 1;
        }
        let child_lines: Vec<_> = lines[child_start..i]
            .iter()
            .map(|s| s.strip_prefix("  ").unwrap_or(s))
            .collect();
        let children_markdown = child_lines.join("\n");
        out.push(Draft {
            source: lines[start..i].join("\n"),
            payload: payloads.pop().unwrap(),
            children_markdown,
        });
    }
    Ok(out)
}

fn rich_text(kind: &NotionBlockKind) -> &[RichTextSpan] {
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
        | NotionBlockKind::Code { rich_text, .. } => rich_text,
        _ => &[],
    }
}

/// Return contiguous matches for unchanged input slices, preserving insertions between slices.
fn matched_range(matches: &[Option<usize>], range: Range<usize>) -> Option<Range<usize>> {
    let values = matches.get(range)?;
    let start = values.first().copied().flatten()?;
    values
        .iter()
        .enumerate()
        .all(|(i, &v)| v == Some(start + i))
        .then_some(start..start + values.len())
}

fn alignment<T: PartialEq>(old: &[T], new: &[T]) -> Vec<Option<usize>> {
    let mut matches = vec![None; old.len()];
    align_range(old, new, 0, &mut matches);
    matches
}

// Hirschberg alignment keeps memory proportional to the input, including broad edits.
fn align_range<T: PartialEq>(old: &[T], new: &[T], offset: usize, matches: &mut [Option<usize>]) {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    for (i, slot) in matches[..prefix].iter_mut().enumerate() {
        *slot = Some(offset + i);
    }
    for (i, slot) in matches[old.len() - suffix..].iter_mut().enumerate() {
        *slot = Some(offset + new.len() - suffix + i);
    }

    let matches = &mut matches[prefix..old.len() - suffix];
    let old = &old[prefix..old.len() - suffix];
    let new = &new[prefix..new.len() - suffix];
    let offset = offset + prefix;
    match (old, new) {
        ([], _) | (_, []) => return,
        ([item], _) => {
            matches[0] = new
                .iter()
                .position(|other| item == other)
                .map(|i| offset + i);
            return;
        }
        _ => {}
    }

    let mid = old.len() / 2;
    let split = {
        let prefix_scores = lcs_lengths(old[..mid].iter(), new.iter());
        let suffix_scores = lcs_lengths(old[mid..].iter().rev(), new.iter().rev());
        (0..=new.len())
            .rev()
            .max_by_key(|&i| prefix_scores[i] + suffix_scores[new.len() - i])
            .unwrap()
    };
    let (left_matches, right_matches) = matches.split_at_mut(mid);
    align_range(&old[..mid], &new[..split], offset, left_matches);
    align_range(&old[mid..], &new[split..], offset + split, right_matches);
}

fn lcs_lengths<'a, T: PartialEq + 'a>(
    old: impl Iterator<Item = &'a T>,
    new: impl ExactSizeIterator<Item = &'a T> + Clone,
) -> Vec<usize> {
    let mut lengths = vec![0; new.len() + 1];
    for item in old {
        let mut diagonal = 0;
        for (j, other) in new.clone().enumerate() {
            let above = lengths[j + 1];
            lengths[j + 1] = if item == other {
                diagonal + 1
            } else {
                lengths[j].max(above)
            };
            diagonal = above;
        }
    }
    lengths
}

pub(crate) fn code_rich_text(text: &str) -> Vec<Value> {
    if text.is_empty() {
        Vec::new()
    } else {
        vec![json!({ "type": "text", "text": { "content": text } })]
    }
}

pub(crate) fn edited_rich_text(original: &[Value], edited: &str) -> Result<Vec<Value>> {
    let spans = parse_rich_text_array(Some(&json!(original)));
    let display = rich_text_to_markdown(&spans);
    let new_chars: Vec<char> = edited.chars().collect();
    let matches = alignment(&display.chars().collect::<Vec<_>>(), &new_chars);
    let styled_ranges: Vec<_> = inline_markdown_spans(edited)
        .into_iter()
        .filter_map(|(range, value)| {
            (value.get("annotations").is_some()
                || value["text"].get("link").is_some()
                || value.get("type").and_then(Value::as_str) == Some("equation"))
                .then_some(range)
        })
        .collect();
    let mut start = 0;
    let mut from = 0;
    let mut result = Vec::new();
    for (span, raw) in spans.iter().zip(original) {
        let end = start
            + rich_text_to_markdown(std::slice::from_ref(span))
                .chars()
                .count();
        if let Some(matched) = matched_range(&matches, start..end)
            .filter(|range| range.start >= from && whole_style_tokens(range, &styled_ranges))
        {
            append_inline(&mut result, &new_chars[from..matched.start]);
            result.push(writable_rich_text(raw, span)?);
            from = matched.end;
        } else if start != end && matches!(span.kind, RichTextKind::Text) {
            let mut template = span.clone();
            template.plain_text = "\0".into();
            let markup = rich_text_to_markdown(&[template]);
            let (prefix, suffix) = markup.split_once('\0').expect("rendered text");
            let matched = matches[start]
                .zip(matches[end - 1])
                .map(|(first, last)| first..last + 1);
            let span_edit = matched
                .filter(|range| {
                    range.start >= from
                        && whole_style_tokens(range, &styled_ranges)
                        && (!prefix.is_empty()
                            || !suffix.is_empty()
                            || styled_ranges.iter().all(|style| !overlaps(range, style)))
                })
                .and_then(|range| {
                    let rendered: String = new_chars[range.clone()].iter().collect();
                    let content = rendered
                        .strip_prefix(prefix)?
                        .strip_suffix(suffix)?
                        .to_owned();
                    Some((range, content))
                });
            if let Some((range, content)) = span_edit {
                append_inline(&mut result, &new_chars[from..range.start]);
                let mut preserved = writable_rich_text(raw, span)?;
                preserved["text"]["content"] = json!(content);
                result.push(preserved);
                from = range.end;
            } else if prefix.is_empty()
                && suffix.is_empty()
                && (span.annotations.underline
                    || span.annotations.color != crate::models::NotionColor::Default)
            {
                // Edits at either end of a plain styled span can erase its outer anchors.
                // Retain the annotations on each contiguous run of surviving characters.
                for range in surviving_runs(&matches[start..end]) {
                    if range.start < from
                        || styled_ranges.iter().any(|style| overlaps(&range, style))
                    {
                        continue;
                    }
                    append_inline(&mut result, &new_chars[from..range.start]);
                    let mut preserved = writable_rich_text(raw, span)?;
                    preserved["text"]["content"] =
                        json!(new_chars[range.clone()].iter().collect::<String>());
                    result.push(preserved);
                    from = range.end;
                }
            }
        }
        start = end;
    }
    append_inline(&mut result, &new_chars[from..]);
    Ok(result)
}

fn overlaps(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start < b.end && b.start < a.end
}

// Keep a Markdown token intact when the edit added formatting around old text.
fn whole_style_tokens(range: &Range<usize>, styles: &[Range<usize>]) -> bool {
    styles.iter().all(|style| {
        !overlaps(range, style) || (range.start <= style.start && style.end <= range.end)
    })
}

fn surviving_runs(matches: &[Option<usize>]) -> Vec<Range<usize>> {
    let mut ranges: Vec<Range<usize>> = Vec::new();
    let mut contiguous = false;
    for &matched in matches {
        if let Some(i) = matched {
            match ranges.last_mut() {
                Some(last) if contiguous && last.end == i => last.end += 1,
                _ => ranges.push(i..i + 1),
            }
        }
        contiguous = matched.is_some();
    }
    ranges
}

fn append_inline(out: &mut Vec<Value>, chars: &[char]) {
    if !chars.is_empty() {
        out.extend(parse_inline_markdown_rich_text(
            &chars.iter().collect::<String>(),
        ));
    }
}

fn span_to_json(span: &RichTextSpan) -> Value {
    let mut value = match &span.kind {
        RichTextKind::Equation { expression } => json!({
            "type": "equation", "equation": { "expression": expression },
        }),
        _ => json!({
            "type": "text", "text": {
                "content": span.plain_text,
                "link": span.href.as_ref().map(|url| json!({ "url": url })),
            },
        }),
    };
    let a = &span.annotations;
    let color = if a.is_background {
        format!("{}_background", a.color.as_str())
    } else {
        a.color.as_str().to_owned()
    };
    value["annotations"] = json!({
        "bold": a.bold, "italic": a.italic, "strikethrough": a.strikethrough,
        "underline": a.underline, "code": a.code, "color": color,
    });
    value["plain_text"] = json!(span.plain_text);
    value["href"] = json!(span.href);
    value
}

fn writable_rich_text(raw: &Value, span: &RichTextSpan) -> Result<Value> {
    let kind = raw["type"].as_str().unwrap_or("text");
    let mut value = json!({ "type": kind, (kind): raw[kind].clone() });
    if let Some(annotations) = raw.get("annotations") {
        value["annotations"] = annotations.clone();
    }
    if let RichTextKind::Mention {
        mention_type,
        target_id: Some(id),
    } = &span.kind
    {
        if matches!(mention_type.as_str(), "user" | "page" | "database") {
            value["mention"][mention_type] = json!({ "id": id });
        }
    }
    if value[kind].is_null() {
        bail!("Cannot write existing rich text of type {kind}");
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{blocks_to_markdown, parse_notion_block};

    fn block(id: &str, kind: &str, text: &str) -> NotionBlock {
        parse_notion_block(
            &json!({
                "id": id, "type": kind,
                (kind): { "rich_text": [{ "type": "text", "text": { "content": text } }] },
            }),
            "page",
            None,
        )
    }

    #[test]
    fn parse_drafts_groups_a_typed_table_into_one_block() {
        let drafts = parse_drafts(&["| A | B |", "| --- | --- |", "| 1 | 2 |", "| 3 | 4 |"]).unwrap();
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].kind(), "table");
        assert_eq!(drafts[0].payload["table"]["table_width"], 2);
        let rows = drafts[0].payload["table"]["children"].as_array().unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|r| r["type"] == "table_row"));
        let payload = parse_drafts(&["| A | B |", "| --- | --- |", "| 1 | 2 |"]).unwrap().remove(0).into_payload().unwrap();
        assert_eq!(payload["table"]["children"].as_array().unwrap().len(), 2, "rows survive into_payload");

        // A lone pipe line is a paragraph, as before.
        let lone = parse_drafts(&["| a | b |"]).unwrap();
        assert_eq!(lone.len(), 1);
        assert_eq!(lone[0].kind(), "paragraph");

        // A table followed by a paragraph is two blocks.
        let two = parse_drafts(&["| A | B |", "| --- | --- |", "| 1 | 2 |", "", "After"]).unwrap();
        assert_eq!(two.iter().map(Draft::kind).collect::<Vec<_>>(), vec!["table", "paragraph"]);

        // A table inside a fenced code block stays code.
        let code = parse_drafts(&["```", "| A | B |", "| --- | --- |", "```"]).unwrap();
        assert_eq!(code.len(), 1);
        assert_eq!(code[0].kind(), "code");
    }

    #[test]
    fn retry_is_safe_only_when_no_one_else_changed_the_page() {
        let base = vec![
            block("b1", "paragraph", "First"),
            block("b2", "paragraph", "Second"),
            block("b3", "paragraph", "Third"),
        ];
        let edit = "First edited\n\nSecond\n\nThird";
        // Identical trees.
        assert!(retry_is_safe(&base, &base, edit));
        // One block deleted remotely: the cause of the 404.
        let deleted = vec![base[1].clone(), base[2].clone()];
        assert!(retry_is_safe(&base, &deleted, edit));
        // A surviving block edited by someone else.
        let mut changed = deleted.clone();
        changed[0] = block("b2", "paragraph", "Second, by a teammate");
        assert!(!retry_is_safe(&base, &changed, edit));
        // A surviving block that already holds our own edited text.
        let mut ours = base.clone();
        ours[0] = block("b1", "paragraph", "First edited");
        assert!(retry_is_safe(&base, &ours, edit));
        // A new block someone else inserted.
        let mut inserted = base.clone();
        inserted.push(block("b4", "paragraph", "Teammate's note"));
        assert!(!retry_is_safe(&base, &inserted, edit));
        // A new block whose text is in the edit: our own insert that landed.
        let mut own_insert = base.clone();
        own_insert.push(block("b5", "paragraph", "First edited"));
        assert!(retry_is_safe(&base, &own_insert, edit));
        // A cached block back under a new ID with the same content.
        let mut reissued = base.clone();
        reissued[1] = block("b2-new", "paragraph", "Second");
        assert!(retry_is_safe(&base, &reissued, "First edited"));
        // A new ID with different content is someone else's block.
        reissued[1] = block("b2-new", "paragraph", "Second, changed");
        assert!(!retry_is_safe(&base, &reissued, "First edited"));
    }

    #[test]
    fn editing_one_sibling_does_not_reparse_nested_or_unsupported_blocks() {
        let mut list = block("list", "bulleted_list_item", "Checklist");
        list.has_children = true;
        list.children
            .push(block("child", "paragraph", "Keep me nested"));
        let blocks = vec![
            list,
            block("opaque", "unsupported", ""),
            block("tail", "paragraph", "Status: READY"),
        ];
        let original = blocks_to_markdown(&blocks);
        assert!(PageEdit::plan("page", &blocks, &original)
            .unwrap()
            .is_empty());
        let plan = PageEdit::plan("page", &blocks, &original.replace("READY", "VERIFIED")).unwrap();
        assert!(plan.inserts.is_empty());
        assert!(plan.archives.is_empty());
        assert_eq!(plan.patches.len(), 1);
        assert_eq!(plan.patches[0].id, "tail");

        let plan = PageEdit::plan(
            "page",
            &blocks,
            &original.replace("Keep me nested", "Still nested"),
        )
        .unwrap();
        assert!(plan.inserts.is_empty());
        assert!(plan.archives.is_empty());
        assert_eq!(plan.patches.len(), 1);
        assert_eq!(plan.patches[0].id, "child");

        let plan =
            PageEdit::plan("page", &blocks, &original.replace("  Keep me nested\n", "")).unwrap();
        assert!(plan.inserts.is_empty());
        assert!(plan.patches.is_empty());
        assert_eq!(plan.archives, ["child"]);
    }

    #[test]
    fn replanning_a_partial_replacement_reuses_its_returned_id() {
        let original = vec![
            block("intro", "paragraph", "First"),
            block("stable", "paragraph", "Keep"),
            block("tail", "paragraph", "Last"),
        ];
        let edited = "## First revised\n\nKeep\n\n## Last revised";
        let plan = PageEdit::plan("page", &original, edited).unwrap();
        assert_eq!(plan.inserts.len(), 2);
        assert!(plan.inserts[0].at_start);
        assert_eq!(plan.archives, ["intro", "tail"]);

        let mut partial = original;
        partial.insert(0, block("first-inserted", "heading_2", "First revised"));
        let retry = PageEdit::plan("page", &partial, edited).unwrap();
        assert_eq!(retry.inserts.len(), 1);
        assert_eq!(retry.inserts[0].after.as_deref(), Some("stable"));
        assert_eq!(retry.archives, ["intro", "tail"]);
    }

    #[test]
    fn partial_rich_text_edit_preserves_native_mention_link_and_annotations() {
        let original = [
            json!({"type":"mention","mention":{"type":"user","user":{"object":"user","id":"actual-id"}},"plain_text":"@Reviewer"}),
            json!({"type":"text","text":{"content":" approval"},"annotations":{"underline":true,"color":"red_background"}}),
            json!({"type":"text","text":{"content":"runbook","link":{"url":"https://example.invalid"}},"annotations":{"italic":true}}),
        ];
        let edited = edited_rich_text(
            &original,
            "@Reviewer approval updated[*runbook*](https://example.invalid)",
        )
        .unwrap();
        assert_eq!(edited[0]["mention"]["user"]["id"], "actual-id");
        assert_eq!(edited[1], original[1]);
        assert_eq!(edited[2]["text"]["content"], " updated");
        assert_eq!(edited[3], original[2]);
    }

    #[test]
    fn adding_markdown_formatting_applies_it_to_existing_text() {
        let original = [json!({"type":"text","text":{"content":"기존 문장"}})];
        let bold = edited_rich_text(&original, "**기존 문장**").unwrap();
        assert_eq!(
            bold,
            [json!({
                "type": "text", "text": { "content": "기존 문장" }, "annotations": { "bold": true },
            })]
        );
        let linked = edited_rich_text(&original, "[기존 문장](https://example.invalid)").unwrap();
        assert_eq!(linked[0]["text"]["link"]["url"], "https://example.invalid");
    }

    #[test]
    fn editing_inside_an_annotated_link_preserves_its_native_formatting() {
        let original = [json!({
            "type": "text",
            "text": { "content": "Alpha BRAVO Omega", "link": { "url": "https://example.invalid/styled" } },
            "annotations": { "underline": true, "color": "red_background" },
        })];
        let edited = edited_rich_text(
            &original,
            "[Alpha DELTA Omega](https://example.invalid/styled)",
        )
        .unwrap();
        let mut expected = original;
        expected[0]["text"]["content"] = json!("Alpha DELTA Omega");
        assert_eq!(edited, expected);
    }

    #[test]
    fn alignment_handles_large_changed_regions() {
        let original: Vec<_> = (0..3000).collect();
        let target: Vec<_> = (5000..7000).chain(1000..1500).chain(7000..7500).collect();
        let matches = alignment(&original, &target);
        assert!(matches[..1000].iter().all(Option::is_none));
        assert!(matches[1500..].iter().all(Option::is_none));
        assert_eq!(
            matches[1000..1500],
            (2000..2500).map(Some).collect::<Vec<_>>()
        );
    }

    fn two_column_blocks() -> Vec<NotionBlock> {
        let mut left_col = parse_notion_block(
            &json!({ "id": "col-left", "type": "column", "has_children": true, "column": {} }),
            "page",
            Some("col-list"),
        );
        left_col.children.push(block("para-left", "paragraph", "LEFT OLD"));

        let mut right_col = parse_notion_block(
            &json!({ "id": "col-right", "type": "column", "has_children": true, "column": {} }),
            "page",
            Some("col-list"),
        );
        right_col.children.push(block("para-right", "paragraph", "RIGHT OLD"));

        let mut col_list = parse_notion_block(
            &json!({ "id": "col-list", "type": "column_list", "has_children": true, "column_list": {} }),
            "page",
            None,
        );
        col_list.children = vec![left_col, right_col];
        vec![col_list]
    }

    #[test]
    fn column_layout_edits_patch_column_paragraphs_in_place() {
        let blocks = two_column_blocks();
        let plain = blocks_to_markdown(&blocks);
        assert!(!plain.contains("<!-- notion:column"));
        let editor_md = blocks_to_editor_markdown(&blocks);
        assert!(editor_md.contains("<!-- notion:column-list id=col-list -->"));
        assert!(editor_md.contains("<!-- notion:column id=col-left -->"));
        assert!(editor_md.contains("<!-- notion:column id=col-right -->"));
        assert!(editor_md.contains("<!-- /notion:column-list -->"));

        assert!(PageEdit::plan("page", &blocks, &editor_md).unwrap().is_empty());
        assert!(PageEdit::plan("page", &blocks, &plain).unwrap().is_empty());

        // 1. Edit only the left column -> 1 PATCH on para-left, 0 inserts, 0 archives
        let left_only = PageEdit::plan(
            "page",
            &blocks,
            &editor_md.replace("LEFT OLD", "LEFT NEW"),
        )
        .unwrap();
        assert!(left_only.inserts.is_empty());
        assert!(left_only.archives.is_empty());
        assert_eq!(left_only.patches.len(), 1);
        assert_eq!(left_only.patches[0].id, "para-left");

        // 2. Edit both columns -> 2 PATCHes (para-left, para-right), 0 inserts, 0 archives
        let both = PageEdit::plan(
            "page",
            &blocks,
            &editor_md
                .replace("LEFT OLD", "LEFT NEW")
                .replace("RIGHT OLD", "RIGHT NEW"),
        )
        .unwrap();
        assert!(both.inserts.is_empty());
        assert!(both.archives.is_empty());
        assert_eq!(both.patches.len(), 2);
        assert_eq!(both.patches[0].id, "para-left");
        assert_eq!(both.patches[1].id, "para-right");
    }

    #[test]
    fn column_layout_rejects_missing_or_corrupted_markers_without_root_insert() {
        let blocks = two_column_blocks();
        let editor_md = blocks_to_editor_markdown(&blocks);

        // Plain markerless edits to both columns must NEVER fall back to root insertion
        assert!(PageEdit::plan("page", &blocks, "LEFT NEW\n\nRIGHT NEW").is_err());
        assert!(PageEdit::plan("page", &blocks, "LEFT NEW\n\nRIGHT OLD").is_err());

        // Broken / missing / swapped / malformed markers all fail cleanly
        assert!(PageEdit::plan(
            "page",
            &blocks,
            &editor_md.replace("<!-- notion:column id=col-right -->\n", ""),
        )
        .is_err());
        assert!(PageEdit::plan(
            "page",
            &blocks,
            &editor_md.replace("<!-- /notion:column-list -->", ""),
        )
        .is_err());
        assert!(PageEdit::plan(
            "page",
            &blocks,
            &editor_md.replace("col-right", "col-wrong"),
        )
        .is_err());
        assert!(PageEdit::plan(
            "page",
            &blocks,
            &editor_md.replace("<!-- notion:column id=col-left -->", "<!-- notion:column id=col-left"),
        )
        .is_err());
    }

    #[test]
    fn column_layout_preserves_surrounding_root_anchors_and_in_column_inserts() {
        let mut blocks = vec![block("top", "paragraph", "Top")];
        blocks.extend(two_column_blocks());
        blocks.push(block("bottom", "paragraph", "Bottom"));

        let editor_md = blocks_to_editor_markdown(&blocks);
        let edited = editor_md
            .replace("Top", "Top revised")
            .replace("LEFT OLD", "LEFT OLD\n\nLEFT SECOND")
            .replace("Bottom", "Bottom revised");
        let plan = PageEdit::plan("page", &blocks, &edited).unwrap();
        assert!(plan.archives.is_empty());
        assert_eq!(plan.patches.len(), 2);
        assert_eq!(plan.patches[0].id, "top");
        assert_eq!(plan.patches[1].id, "bottom");
        assert_eq!(plan.inserts.len(), 1);
        assert_eq!(plan.inserts[0].parent, "col-left");
        assert_eq!(plan.inserts[0].after.as_deref(), Some("para-left"));
        assert!(!plan.inserts[0].at_start);
    }
}
