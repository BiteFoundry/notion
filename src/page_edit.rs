use std::{borrow::Cow, collections::HashSet, ops::Range};

use anyhow::{bail, ensure, Result};
use serde_json::{json, Value};

use crate::{
    models::{NotionBlock, NotionBlockKind, RichTextKind, RichTextSpan},
    notion::NotionClient,
    parser::{
        is_preserved_anchor_block, markdown_to_notion_blocks,
        notion::{
            block_inline_markdown, inline_markdown_spans, markdown_edit_ranges,
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
    children: Vec<Draft>,
}

impl Draft {
    fn kind(&self) -> &str {
        self.payload["type"].as_str().expect("Markdown block type")
    }

    fn into_payload(mut self) -> Value {
        if !self.children.is_empty() {
            let kind = self.kind().to_owned();
            self.payload[&kind]["children"] =
                Value::Array(self.children.into_iter().map(Self::into_payload).collect());
        }
        self.payload
    }
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
        let (original, ranges) = markdown_edit_ranges(old);
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
        let mut after = None;
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
        if let [block] = old {
            let previous = rich_text(&block.kind);
            let rendered = rich_text_to_markdown(previous);
            if block.block_type == "paragraph" && rendered.contains('\n') {
                let edited = lines.join("\n");
                let text = edited.trim_matches('\n');
                let same_paragraph = text.lines().count() == rendered.lines().count()
                    && markdown_to_notion_blocks(text)
                        .iter()
                        .all(|value| value["type"] == "paragraph");
                if same_paragraph {
                    self.patches.push(BlockPatch {
                        id: block.id.clone(),
                        kind: block.block_type.clone(),
                        fields: serde_json::Map::new(),
                        text: Some((previous.to_vec(), text.to_owned())),
                    });
                    *after = Some(block.id.clone());
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
                    new_blocks.extend(drafts.by_ref().map(Draft::into_payload));
                }
                self.queue_insertion(parent, after, &mut new_blocks);
                *after = Some(block.id.clone());
                continue;
            }
            if let Some(draft) = drafts.peek() {
                if draft.kind() == block.block_type && patchable(&block.block_type) {
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
                    new_blocks.push(drafts.next().unwrap().into_payload());
                }
            }
            // Hidden children (e.g. depth-capped reads) and structural blocks keep their IDs.
            if !block.has_children && block.children.is_empty() {
                self.archives.push(block.id.clone());
            } else {
                *after = Some(block.id.clone());
            }
        }
        new_blocks.extend(drafts.map(Draft::into_payload));
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
        let kind = draft.kind();
        let mut fields = serde_json::Map::new();
        let mut text = None;
        let desired = &draft.payload[kind];
        let previous = rich_text(&block.kind);
        if kind == "code" {
            let new_text = parse_rich_text_array(desired.get("rich_text"));
            if crate::models::spans_plain_text(previous)
                != crate::models::spans_plain_text(&new_text)
            {
                fields.insert("rich_text".into(), desired["rich_text"].clone());
            }
        } else {
            let inline =
                block_inline_markdown(kind, draft.source.lines().next().unwrap_or_default());
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
                if desired["icon"]["emoji"].as_str() != icon.as_deref()
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
            "bulleted_list_item" | "numbered_list_item" | "to_do" | "toggle" | "callout"
        );
        if !draft.children.is_empty() || (renders_children && !block.children.is_empty()) {
            ensure!(
                !block.has_children || !block.children.is_empty(),
                "Children of block {} were not loaded",
                block.id
            );
            let markdown = draft
                .children
                .iter()
                .map(|draft| draft.source.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            self.siblings(&block.id, &block.children, &markdown)?;
        }
        Ok(())
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
                && (opaque(a) || a.kind == b.kind)
                && same_edit_baseline(&a.children, &b.children)
        })
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
            ![
                "[unsupported:",
                "[📎 ",
                "[📄 ",
                "[🎬 ",
                "[🎵 ",
                "📄 **",
                "🗂 **",
                "[Table of Contents]",
                "[⚡ "
            ]
            .iter()
            .any(|prefix| line.trim_start().starts_with(prefix)),
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
        let head = lines[start..i].join("\n");
        let mut payloads = markdown_to_notion_blocks(&head);
        ensure!(payloads.len() == 1, "Cannot parse an edited Markdown block");
        let child_start = i;
        while i < lines.len() && (lines[i].trim().is_empty() || lines[i].starts_with("  ")) {
            i += 1;
        }
        let child_lines: Vec<_> = lines[child_start..i]
            .iter()
            .map(|s| s.strip_prefix("  ").unwrap_or(s))
            .collect();
        let children = parse_drafts(&child_lines)?;
        out.push(Draft {
            source: lines[start..i].join("\n"),
            payload: payloads.pop().unwrap(),
            children,
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

pub(crate) fn edited_rich_text(original: &[Value], edited: &str) -> Result<Vec<Value>> {
    let spans = parse_rich_text_array(Some(&json!(original)));
    let display = rich_text_to_markdown(&spans);
    let new_chars: Vec<char> = edited.chars().collect();
    let matches = alignment(&display.chars().collect::<Vec<_>>(), &new_chars);
    let styled_ranges: Vec<_> = inline_markdown_spans(edited)
        .into_iter()
        .filter_map(|(range, value)| {
            (value.get("annotations").is_some() || value["text"].get("link").is_some())
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
}
