//! Keeps every text span of a write inside Notion's request limits.
//!
//! Notion rejects a `text.content` longer than 2,000 characters and a `rich_text` array longer
//! than 100 items with HTTP 400. The client runs [`chunk_payload`] on each write body, which
//! splits long spans into adjacent spans with identical formatting, so the text reads back
//! unchanged.

use anyhow::Result;
use serde_json::Value;
use unicode_segmentation::UnicodeSegmentation;

use crate::models::coded;

/// Longest `text.content` accepted per span, counted in UTF-16 code units. A chunk within this
/// limit is also within 2,000 Unicode scalar values, so it is valid whichever unit Notion counts.
pub const TEXT_CONTENT_LIMIT: usize = 2000;
/// Most spans accepted in one `rich_text` array.
pub const RICH_TEXT_ARRAY_LIMIT: usize = 100;

fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// Splits `text` into slices of at most `limit` UTF-16 code units.
///
/// Cuts fall on grapheme-cluster boundaries, so an emoji sequence or a letter with its combining
/// marks stays in one chunk; a single cluster longer than the limit is cut on `char` boundaries.
/// Concatenating the result gives back `text` exactly. Empty input yields one empty slice.
pub fn split_text(text: &str, limit: usize) -> Vec<&str> {
    let limit = limit.max(1);
    if utf16_len(text) <= limit {
        return vec![text];
    }
    let mut chunks = Vec::new();
    let mut start = 0usize;
    let mut units = 0usize;
    for (offset, grapheme) in text.grapheme_indices(true) {
        let grapheme_units = utf16_len(grapheme);
        if units + grapheme_units <= limit {
            units += grapheme_units;
            continue;
        }
        if offset > start {
            chunks.push(&text[start..offset]);
            start = offset;
        }
        units = 0;
        if grapheme_units <= limit {
            units = grapheme_units;
            continue;
        }
        for (char_offset, ch) in grapheme.char_indices() {
            let at = offset + char_offset;
            if units + ch.len_utf16() > limit && at > start {
                chunks.push(&text[start..at]);
                start = at;
                units = 0;
            }
            units += ch.len_utf16();
        }
    }
    if start < text.len() {
        chunks.push(&text[start..]);
    }
    chunks
}

/// The text of a span that may be split: `"type": "text"`, or no type with a `text.content`.
/// Mentions and equations are atomic and are never split.
fn splittable_content(item: &Value) -> Option<&str> {
    match item.get("type").and_then(Value::as_str) {
        None | Some("text") => item.get("text")?.get("content")?.as_str(),
        Some(_) => None,
    }
}

fn needs_split(item: &Value) -> bool {
    splittable_content(item).is_some_and(|content| utf16_len(content) > TEXT_CONTENT_LIMIT)
}

/// Splits every over-long text span into adjacent copies of the whole span object.
///
/// Only `text.content` (and `plain_text` when present) differs between the copies, so
/// annotations, links and unknown keys apply to every chunk. Other items pass through unchanged.
pub fn chunk_rich_text(items: &[Value]) -> Vec<Value> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let content = match splittable_content(item) {
            Some(content) if utf16_len(content) > TEXT_CONTENT_LIMIT => content,
            _ => {
                out.push(item.clone());
                continue;
            }
        };
        for piece in split_text(content, TEXT_CONTENT_LIMIT) {
            let mut span = item.clone();
            span["text"]["content"] = Value::String(piece.to_string());
            if span.get("plain_text").is_some_and(Value::is_string) {
                span["plain_text"] = Value::String(piece.to_string());
            }
            out.push(span);
        }
    }
    out
}

fn chunk_array_in_place(value: &mut Value) -> Result<()> {
    let Value::Array(items) = value else {
        return Ok(());
    };
    // Untouched arrays keep their exact JSON, so short payloads are sent as before.
    if !items.iter().any(needs_split) {
        return Ok(());
    }
    let chunked = chunk_rich_text(items);
    if chunked.len() > RICH_TEXT_ARRAY_LIMIT {
        return Err(coded(
            "invalid_input",
            "Text is too long for one Notion block (over 200,000 characters). Split it into several blocks.",
        ));
    }
    *items = chunked;
    Ok(())
}

/// Applies [`chunk_rich_text`] to every rich-text array of a write body: `rich_text`, `caption`,
/// `title`, `description` and each cell of `cells`, at any depth (block children, page properties).
///
/// Values under those keys that are not arrays, such as a database schema's `"title": {}`, are
/// left alone. Not meant for search or query bodies, where `rich_text` is a filter object.
pub fn chunk_payload(payload: &mut Value) -> Result<()> {
    match payload {
        Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                match key.as_str() {
                    "rich_text" | "caption" | "title" | "description" => {
                        chunk_array_in_place(value)?;
                    }
                    "cells" => {
                        if let Value::Array(cells) = value {
                            for cell in cells {
                                chunk_array_in_place(cell)?;
                            }
                        }
                    }
                    _ => {}
                }
                chunk_payload(value)?;
            }
        }
        Value::Array(items) => {
            for item in items {
                chunk_payload(item)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn span(text: &str) -> Value {
        json!({ "type": "text", "text": { "content": text } })
    }

    fn contents(items: &[Value]) -> Vec<String> {
        items
            .iter()
            .map(|item| item["text"]["content"].as_str().unwrap().to_string())
            .collect()
    }

    fn assert_valid_split(text: &str, limit: usize) -> Vec<&str> {
        let chunks = split_text(text, limit);
        assert_eq!(chunks.concat(), text, "chunks must reproduce the input");
        for chunk in &chunks {
            assert!(utf16_len(chunk) <= limit, "chunk has {} units", utf16_len(chunk));
        }
        chunks
    }

    #[test]
    fn split_text_ascii_boundaries() {
        let exact = "a".repeat(2000);
        assert_eq!(split_text(&exact, TEXT_CONTENT_LIMIT), vec![exact.as_str()]);
        let over = "a".repeat(2001);
        let chunks = assert_valid_split(&over, TEXT_CONTENT_LIMIT);
        assert_eq!(chunks.iter().map(|c| c.len()).collect::<Vec<_>>(), vec![2000, 1]);
        assert_eq!(split_text("", TEXT_CONTENT_LIMIT), vec![""]);
    }

    #[test]
    fn split_text_hangul_and_astral_emoji() {
        let short = "가".repeat(1500);
        assert_eq!(split_text(&short, TEXT_CONTENT_LIMIT).len(), 1);
        let long = "한".repeat(2500);
        let chunks = assert_valid_split(&long, TEXT_CONTENT_LIMIT);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].chars().count(), 2000);

        // 1,001 emoji outside the BMP are 2,002 UTF-16 units but only 1,001 scalars.
        let emoji = "😀".repeat(1001);
        let chunks = assert_valid_split(&emoji, TEXT_CONTENT_LIMIT);
        assert_eq!(chunks.len(), 2);
        assert_eq!(utf16_len(chunks[0]), 2000);
        assert_eq!(chunks[1], "😀");
    }

    #[test]
    fn split_text_keeps_grapheme_clusters_whole() {
        let family = "👨‍👩‍👧‍👦"; // 11 UTF-16 units joined by ZWJ
        let text = format!("{}{family}{}", "a".repeat(1995), "b".repeat(10));
        let chunks = assert_valid_split(&text, TEXT_CONTENT_LIMIT);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], "a".repeat(1995));
        assert!(chunks[1].starts_with(family));
        assert_eq!(chunks.iter().filter(|c| c.contains(family)).count(), 1);

        // "e" + combining acute accent straddling the limit moves to the next chunk together.
        let accented = format!("{}e\u{301}tail", "a".repeat(1999));
        let chunks = assert_valid_split(&accented, TEXT_CONTENT_LIMIT);
        assert_eq!(chunks[0], "a".repeat(1999));
        assert!(chunks[1].starts_with("e\u{301}"));
    }

    #[test]
    fn split_text_cuts_an_oversized_cluster_on_char_boundaries() {
        // One base letter with 30 combining marks is a single cluster of 31 units.
        let cluster = format!("e{}", "\u{301}".repeat(30));
        let chunks = assert_valid_split(&cluster, 8);
        assert!(chunks.len() >= 4);
        let astral = "😀😀😀";
        let chunks = assert_valid_split(astral, 3);
        assert_eq!(chunks, vec!["😀", "😀", "😀"], "a surrogate pair is never split");
    }

    #[test]
    fn split_text_mixed_strings_hold_the_invariant() {
        let parts = ["plain ascii ", "한글 텍스트 ", "😀👍🏽", "e\u{301}", "👨‍👩‍👧‍👦", "\n\t", "日本語"];
        for seed in 0..40usize {
            let mut text = String::new();
            let mut i = seed;
            while utf16_len(&text) < 4500 + seed * 7 {
                text.push_str(parts[i % parts.len()]);
                i = i.wrapping_mul(31).wrapping_add(7);
            }
            for limit in [2000, 1999, 100, 17] {
                assert_valid_split(&text, limit);
            }
        }
    }

    #[test]
    fn chunk_rich_text_copies_formatting_to_every_chunk() {
        let item = json!({
            "type": "text",
            "text": { "content": "x".repeat(4500), "link": { "url": "https://example.com" } },
            "annotations": { "bold": true, "color": "red_background" },
            "plain_text": "x".repeat(4500),
            "href": "https://example.com",
            "custom_key": 7
        });
        let chunks = chunk_rich_text(std::slice::from_ref(&item));
        assert_eq!(chunks.len(), 3);
        assert_eq!(
            contents(&chunks).iter().map(String::len).collect::<Vec<_>>(),
            vec![2000, 2000, 500]
        );
        for chunk in &chunks {
            assert_eq!(chunk["annotations"], item["annotations"]);
            assert_eq!(chunk["text"]["link"], item["text"]["link"]);
            assert_eq!(chunk["href"], item["href"]);
            assert_eq!(chunk["custom_key"], 7);
            assert_eq!(chunk["plain_text"], chunk["text"]["content"]);
        }
    }

    #[test]
    fn chunk_rich_text_leaves_short_spans_mentions_and_equations_untouched() {
        let long = "m".repeat(3000);
        let items = vec![
            span("short"),
            json!({ "type": "mention", "mention": { "page": { "id": "p1" } }, "plain_text": long }),
            json!({ "type": "equation", "equation": { "expression": long } }),
            json!({ "text": { "content": "typeless" } }),
        ];
        assert_eq!(chunk_rich_text(&items), items);
        // A span with no "type" but a long text.content is still split.
        let typeless = vec![json!({ "text": { "content": "t".repeat(2001) } })];
        assert_eq!(chunk_rich_text(&typeless).len(), 2);
    }

    #[test]
    fn chunk_payload_walks_blocks_cells_captions_and_properties() {
        let long = "z".repeat(2500);
        let mut payload = json!({
            "parent": { "page_id": "p" },
            "properties": {
                "Name": { "title": [span(&long)] },
                "Notes": { "rich_text": [span(&long)] }
            },
            "children": [
                {
                    "type": "toggle",
                    "toggle": {
                        "rich_text": [span("head")],
                        "children": [{ "type": "paragraph", "paragraph": { "rich_text": [span(&long)] } }]
                    }
                },
                { "type": "table_row", "table_row": { "cells": [[span(&long)], [span("ok")]] } },
                { "type": "image", "image": { "external": { "url": "https://x" }, "caption": [span(&long)] } }
            ]
        });
        chunk_payload(&mut payload).unwrap();
        let lens = |pointer: &str| payload.pointer(pointer).unwrap().as_array().unwrap().len();
        assert_eq!(lens("/properties/Name/title"), 2);
        assert_eq!(lens("/properties/Notes/rich_text"), 2);
        assert_eq!(lens("/children/0/toggle/rich_text"), 1);
        assert_eq!(lens("/children/0/toggle/children/0/paragraph/rich_text"), 2);
        assert_eq!(lens("/children/1/table_row/cells/0"), 2);
        assert_eq!(lens("/children/1/table_row/cells/1"), 1);
        assert_eq!(lens("/children/2/image/caption"), 2);
    }

    #[test]
    fn chunk_payload_keeps_short_payloads_and_schemas_identical() {
        let mut schema = json!({
            "parent": { "page_id": "p" },
            "title": [span("Tasks")],
            "description": [span("Tracker")],
            "properties": {
                "Name": { "title": {} },
                "Notes": { "rich_text": {} },
                "title": { "title": {} }
            }
        });
        let before = schema.clone();
        chunk_payload(&mut schema).unwrap();
        assert_eq!(schema, before);

        let mut db = json!({ "title": [span(&"d".repeat(2100))], "description": [span(&"e".repeat(2100))] });
        chunk_payload(&mut db).unwrap();
        assert_eq!(db["title"].as_array().unwrap().len(), 2);
        assert_eq!(db["description"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn chunk_payload_rejects_text_beyond_one_block() {
        let mut payload = json!({
            "children": [{ "type": "code", "code": { "rich_text": [span(&"c".repeat(250_000))] } }]
        });
        let err = chunk_payload(&mut payload).unwrap_err();
        assert!(err.to_string().contains("too long for one Notion block"), "{err}");
        let coded = err.downcast_ref::<crate::models::CodedError>().unwrap();
        assert_eq!(coded.code, "invalid_input");

        // Exactly 100 spans of 2,000 units still fits.
        let mut fits = json!({ "code": { "rich_text": [span(&"c".repeat(200_000))] } });
        chunk_payload(&mut fits).unwrap();
        assert_eq!(fits["code"]["rich_text"].as_array().unwrap().len(), 100);
    }
}
