//! Notion Block & Rich-Text Engine:
//! - Parses Notion REST API v1 JSON into strongly typed Pages, Databases, Properties, Blocks, Rich-Text Spans, and Comments.
//! - Exports Notion block trees into clean GitHub-Flavored Markdown (`blocks_to_markdown`).
//! - Converts Markdown or slash-style quick-append input (`/h1`, `/todo`, `/callout`, `/code`, `/quote`, `/toggle`)
//!   into valid Notion API block payloads (`markdown_to_notion_blocks`, `parse_slash_command_blocks`).

use std::collections::BTreeMap;

use anyhow::{anyhow, Result};
use bitefoundry_html::{sniff_content_kind, ContentKind};
use serde_json::{json, Value};
use unicode_segmentation::UnicodeSegmentation;

use crate::models::{
    spans_plain_text, NotionBlock, NotionBlockKind, NotionColor, NotionComment, NotionDatabase,
    NotionMediaRef, NotionPage, NotionPageDetail, NotionPropertySchema, NotionPropertyValue,
    RichTextAnnotations, RichTextKind, RichTextSpan, SelectOption,
};

/// Parses a Notion `rich_text` JSON array into typed `RichTextSpan` entries.
pub fn parse_rich_text_array(val: Option<&Value>) -> Vec<RichTextSpan> {
    let Some(arr) = val.and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter().map(parse_rich_text_item).collect()
}

fn parse_rich_text_item(item: &Value) -> RichTextSpan {
    let plain_text = item
        .get("plain_text")
        .and_then(Value::as_str)
        .or_else(|| {
            item.get("text")
                .and_then(|t| t.get("content"))
                .and_then(Value::as_str)
        })
        .unwrap_or_default()
        .to_string();

    let href = item
        .get("href")
        .and_then(Value::as_str)
        .or_else(|| {
            item.get("text")
                .and_then(|t| t.get("link"))
                .and_then(|l| l.get("url"))
                .and_then(Value::as_str)
        })
        .map(str::to_string);

    let ann = item.get("annotations");
    let raw_color = ann.and_then(|a| a.get("color")).and_then(Value::as_str);
    let annotations = RichTextAnnotations {
        bold: ann
            .and_then(|a| a.get("bold"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        italic: ann
            .and_then(|a| a.get("italic"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        strikethrough: ann
            .and_then(|a| a.get("strikethrough"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        underline: ann
            .and_then(|a| a.get("underline"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        code: ann
            .and_then(|a| a.get("code"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        color: NotionColor::from_notion_str(raw_color),
        is_background: raw_color.is_some_and(|c| c.ends_with("_background")),
    };

    let rt_type = item.get("type").and_then(Value::as_str).unwrap_or("text");
    let kind = match rt_type {
        "mention" => {
            let mention = item.get("mention");
            let mtype = mention
                .and_then(|m| m.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let target_id = mention
                .and_then(|m| m.get(&mtype))
                .and_then(|inner| inner.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string);
            RichTextKind::Mention {
                mention_type: mtype,
                target_id,
            }
        }
        "equation" => {
            let expr = item
                .get("equation")
                .and_then(|e| e.get("expression"))
                .and_then(Value::as_str)
                .unwrap_or(&plain_text)
                .to_string();
            RichTextKind::Equation { expression: expr }
        }
        _ => RichTextKind::Text,
    };

    RichTextSpan {
        plain_text,
        href,
        annotations,
        kind,
    }
}

fn parse_icon(val: Option<&Value>) -> Option<String> {
    let icon = val?;
    match icon.get("type").and_then(Value::as_str) {
        Some("emoji") => icon.get("emoji").and_then(Value::as_str).map(str::to_string),
        Some("external") => icon
            .get("external")
            .and_then(|e| e.get("url"))
            .and_then(Value::as_str)
            .map(|_| "🖼".to_string()),
        Some("file") => Some("🖼".to_string()),
        _ => None,
    }
}

fn parse_cover_url(val: Option<&Value>) -> Option<String> {
    let cover = val?;
    match cover.get("type").and_then(Value::as_str) {
        Some("external") => cover
            .get("external")
            .and_then(|e| e.get("url"))
            .and_then(Value::as_str)
            .map(str::to_string),
        Some("file") => cover
            .get("file")
            .and_then(|f| f.get("url"))
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => None,
    }
}

pub fn parse_parent(val: Option<&Value>) -> (String, Option<String>) {
    let Some(parent) = val else {
        return ("workspace".to_string(), None);
    };
    let ptype = parent
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("workspace")
        .to_string();
    let pid = match ptype.as_str() {
        "page_id" => parent.get("page_id").and_then(Value::as_str),
        "database_id" => parent.get("database_id").and_then(Value::as_str),
        "block_id" => parent.get("block_id").and_then(Value::as_str),
        _ => None,
    }
    .map(str::to_string);
    (ptype, pid)
}

fn parse_user_ref(val: Option<&Value>) -> Option<String> {
    let u = val?;
    if let Some(s) = u.as_str().map(str::trim).filter(|s| !s.is_empty()) {
        return Some(s.to_string());
    }
    u.get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            u.get("id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
        .map(str::to_string)
}

fn summarize_json_value(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        Value::Array(arr) if arr.is_empty() => String::new(),
        Value::Object(map) if map.is_empty() => String::new(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

fn parse_select_option(val: &Value) -> SelectOption {
    SelectOption {
        id: val.get("id").and_then(Value::as_str).map(str::to_string),
        name: val
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        color: NotionColor::from_notion_str(val.get("color").and_then(Value::as_str)),
    }
}

pub fn parse_property_value(val: &Value) -> NotionPropertyValue {
    let inferred = if val.get("title").is_some() {
        "title"
    } else if val.get("rich_text").is_some() {
        "rich_text"
    } else if val.get("status").is_some() {
        "status"
    } else if val.get("select").is_some() {
        "select"
    } else if val.get("multi_select").is_some() {
        "multi_select"
    } else if val.get("checkbox").is_some() {
        "checkbox"
    } else {
        ""
    };
    let ptype = val.get("type").and_then(Value::as_str).unwrap_or(inferred);
    match ptype {
        "title" => NotionPropertyValue::Title(spans_plain_text(&parse_rich_text_array(
            val.get("title"),
        ))),
        "rich_text" => NotionPropertyValue::RichText(spans_plain_text(&parse_rich_text_array(
            val.get("rich_text"),
        ))),
        "status" => NotionPropertyValue::Status(
            val.get("status")
                .filter(|v| v.is_object())
                .map(parse_select_option),
        ),
        "select" => NotionPropertyValue::Select(
            val.get("select")
                .filter(|v| v.is_object())
                .map(parse_select_option),
        ),
        "multi_select" => NotionPropertyValue::MultiSelect(
            val.get("multi_select")
                .and_then(Value::as_array)
                .map(|arr| arr.iter().map(parse_select_option).collect())
                .unwrap_or_default(),
        ),
        "date" => {
            let d = val.get("date");
            NotionPropertyValue::Date {
                start: d
                    .and_then(|x| x.get("start"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                end: d
                    .and_then(|x| x.get("end"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
            }
        }
        "checkbox" => NotionPropertyValue::Checkbox(
            val.get("checkbox").and_then(Value::as_bool).unwrap_or(false),
        ),
        "number" => NotionPropertyValue::Number(val.get("number").and_then(Value::as_f64)),
        "people" => NotionPropertyValue::People(
            val.get("people")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|u| {
                            u.get("name")
                                .and_then(Value::as_str)
                                .or_else(|| u.get("id").and_then(Value::as_str))
                                .map(str::to_string)
                        })
                        .collect()
                })
                .unwrap_or_default(),
        ),
        "url" => NotionPropertyValue::Url(val.get("url").and_then(Value::as_str).map(str::to_string)),
        "email" => {
            NotionPropertyValue::Email(val.get("email").and_then(Value::as_str).map(str::to_string))
        }
        "phone_number" => NotionPropertyValue::PhoneNumber(
            val.get("phone_number")
                .and_then(Value::as_str)
                .map(str::to_string),
        ),
        "files" => NotionPropertyValue::Files(
            val.get("files")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|f| {
                            let name = f
                                .get("name")
                                .and_then(Value::as_str)
                                .map(str::trim)
                                .filter(|s| !s.is_empty());
                            let (extracted_url, _) = extract_media_url(Some(f));
                            let url = if !extracted_url.trim().is_empty() {
                                Some(extracted_url.trim().to_string())
                            } else {
                                f.get("url")
                                    .and_then(Value::as_str)
                                    .map(str::trim)
                                    .filter(|s| !s.is_empty())
                                    .map(str::to_string)
                            };
                            match (name, url) {
                                (Some(n), Some(u)) => Some(format!("[{n}]({u})")),
                                (Some(n), None) => Some(n.to_string()),
                                (None, Some(u)) => Some(u),
                                (None, None) => None,
                            }
                        })
                        .collect()
                })
                .unwrap_or_default(),
        ),
        "relation" => NotionPropertyValue::Relation(
            val.get("relation")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|r| r.get("id").and_then(Value::as_str).map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
        ),
        "formula" => {
            let f = val.get("formula");
            let s = f
                .and_then(|x| {
                    x.get("string")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or_else(|| x.get("number").and_then(Value::as_f64).map(|n| n.to_string()))
                        .or_else(|| {
                            x.get("boolean")
                                .and_then(Value::as_bool)
                                .map(|b| b.to_string())
                        })
                        .or_else(|| {
                            x.get("date")
                                .and_then(|d| d.get("start"))
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        })
                })
                .unwrap_or_default();
            NotionPropertyValue::Formula(s)
        }
        "rollup" => {
            let s = val
                .get("rollup")
                .map(|r| match r.get("type").and_then(Value::as_str) {
                    Some("number") => r
                        .get("number")
                        .and_then(Value::as_f64)
                        .map(|n| n.to_string())
                        .unwrap_or_default(),
                    Some("date") => {
                        let d = r.get("date");
                        NotionPropertyValue::Date {
                            start: d
                                .and_then(|x| x.get("start"))
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            end: d
                                .and_then(|x| x.get("end"))
                                .and_then(Value::as_str)
                                .map(str::to_string),
                        }
                        .to_plain_string()
                    }
                    Some("array") => r
                        .get("array")
                        .and_then(Value::as_array)
                        .map(|arr| {
                            arr.iter()
                                .map(|elem| {
                                    parse_property_value(elem)
                                        .to_plain_string()
                                        .trim()
                                        .to_string()
                                })
                                .filter(|s| !s.is_empty())
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default(),
                    other => {
                        let payload = other
                            .and_then(|key| r.get(key))
                            .or_else(|| r.get("string"))
                            .or_else(|| r.get("boolean"));
                        match payload {
                            Some(v) => summarize_json_value(v),
                            None => {
                                if let Some(obj) = r.as_object() {
                                    let extras: serde_json::Map<String, Value> = obj
                                        .iter()
                                        .filter(|(k, v)| {
                                            !matches!(k.as_str(), "type" | "function")
                                                && !v.is_null()
                                        })
                                        .map(|(k, v)| (k.clone(), v.clone()))
                                        .collect();
                                    if extras.is_empty() {
                                        String::new()
                                    } else {
                                        serde_json::to_string(&Value::Object(extras))
                                            .unwrap_or_default()
                                    }
                                } else {
                                    summarize_json_value(r)
                                }
                            }
                        }
                    }
                })
                .unwrap_or_default();
            NotionPropertyValue::Rollup(s)
        }
        "created_time" => NotionPropertyValue::CreatedTime(
            val.get("created_time")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        ),
        "last_edited_time" => NotionPropertyValue::LastEditedTime(
            val.get("last_edited_time")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        ),
        "unique_id" => {
            let uid = val.get("unique_id");
            let prefix = uid.and_then(|u| u.get("prefix")).and_then(Value::as_str);
            let num = uid.and_then(|u| u.get("number")).and_then(Value::as_i64);
            let formatted = match (prefix, num) {
                (Some(p), Some(n)) => format!("{p}-{n}"),
                (None, Some(n)) => n.to_string(),
                _ => String::new(),
            };
            NotionPropertyValue::UniqueId(formatted)
        }
        "created_by" | "last_edited_by" => {
            let s = val
                .get(ptype)
                .and_then(|u| {
                    u.get("name")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .or_else(|| u.get("id").and_then(Value::as_str))
                })
                .unwrap_or_default()
                .to_string();
            NotionPropertyValue::Other(s)
        }
        "verification" => {
            let s = val
                .get("verification")
                .and_then(|v| v.get("state"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            NotionPropertyValue::Other(s)
        }
        other => {
            let payload = if !other.is_empty() {
                val.get(other)
            } else {
                None
            };
            let summary = match payload {
                Some(v) => summarize_json_value(v),
                None => {
                    if let Some(obj) = val.as_object() {
                        let extras: serde_json::Map<String, Value> = obj
                            .iter()
                            .filter(|(k, v)| !matches!(k.as_str(), "id" | "type") && !v.is_null())
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect();
                        if extras.is_empty() {
                            String::new()
                        } else {
                            serde_json::to_string(&Value::Object(extras)).unwrap_or_default()
                        }
                    } else {
                        summarize_json_value(val)
                    }
                }
            };
            NotionPropertyValue::Other(summary)
        }
    }
}

/// Parses a Notion API `page` JSON object into `NotionPage`.
pub fn parse_notion_page(val: &Value, workspace_id: &str) -> Result<NotionPage> {
    let id = val
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Notion page JSON missing `id`"))?
        .to_string();
    let (parent_type, parent_id) = parse_parent(val.get("parent"));

    let mut properties = BTreeMap::new();
    let mut title = String::new();
    if let Some(props) = val.get("properties").and_then(Value::as_object) {
        for (name, pval) in props {
            let parsed = parse_property_value(pval);
            if let NotionPropertyValue::Title(ref t) = parsed {
                if title.is_empty() && !t.trim().is_empty() {
                    title = t.clone();
                }
            }
            properties.insert(name.clone(), parsed);
        }
    }
    if title.is_empty() {
        title = "Untitled".to_string();
    }

    Ok(NotionPage {
        id,
        workspace_id: workspace_id.to_string(),
        parent_type,
        parent_id,
        title,
        icon: parse_icon(val.get("icon")),
        cover_url: parse_cover_url(val.get("cover")),
        url: val
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        archived: val
            .get("archived")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || val
                .get("in_trash")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        is_favorite: false,
        properties,
        plain_text: String::new(),
        created_time: val
            .get("created_time")
            .and_then(Value::as_str)
            .map(str::to_string),
        last_edited_time: val
            .get("last_edited_time")
            .and_then(Value::as_str)
            .map(str::to_string),
        created_by: parse_user_ref(val.get("created_by")),
        last_edited_by: parse_user_ref(val.get("last_edited_by")),
        synced_at: Some(chrono::Utc::now().to_rfc3339()),
    })
}

/// Parses a Notion API `database` JSON object into `NotionDatabase`.
pub fn parse_notion_database(val: &Value, workspace_id: &str) -> Result<NotionDatabase> {
    let id = val
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Notion database JSON missing `id`"))?
        .to_string();
    let (parent_type, parent_id) = parse_parent(val.get("parent"));
    let title = spans_plain_text(&parse_rich_text_array(val.get("title")));
    let description = spans_plain_text(&parse_rich_text_array(val.get("description")));

    let mut schema = BTreeMap::new();
    if let Some(props) = val.get("properties").and_then(Value::as_object) {
        for (name, pval) in props {
            let prop_type = pval
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("rich_text")
                .to_string();
            let options = match prop_type.as_str() {
                "status" => pval
                    .get("status")
                    .and_then(|s| s.get("options"))
                    .and_then(Value::as_array)
                    .map(|arr| arr.iter().map(parse_select_option).collect())
                    .unwrap_or_default(),
                "select" => pval
                    .get("select")
                    .and_then(|s| s.get("options"))
                    .and_then(Value::as_array)
                    .map(|arr| arr.iter().map(parse_select_option).collect())
                    .unwrap_or_default(),
                "multi_select" => pval
                    .get("multi_select")
                    .and_then(|s| s.get("options"))
                    .and_then(Value::as_array)
                    .map(|arr| arr.iter().map(parse_select_option).collect())
                    .unwrap_or_default(),
                _ => Vec::new(),
            };
            let relation_database_id = if prop_type == "relation" {
                pval.get("relation")
                    .and_then(|r| r.get("database_id"))
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_string)
            } else {
                None
            };
            schema.insert(
                name.clone(),
                NotionPropertySchema {
                    id: pval
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or(name)
                        .to_string(),
                    name: name.clone(),
                    prop_type,
                    options,
                    relation_database_id,
                },
            );
        }
    }

    Ok(NotionDatabase {
        id,
        workspace_id: workspace_id.to_string(),
        parent_type,
        parent_id,
        title: if title.trim().is_empty() {
            "Untitled Database".to_string()
        } else {
            title
        },
        icon: parse_icon(val.get("icon")),
        description,
        schema,
        url: val
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        archived: val
            .get("archived")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || val
                .get("in_trash")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        is_favorite: false,
        last_edited_time: val
            .get("last_edited_time")
            .and_then(Value::as_str)
            .map(str::to_string),
        synced_at: Some(chrono::Utc::now().to_rfc3339()),
    })
}

fn extract_media_url(obj: Option<&Value>) -> (String, bool) {
    let Some(o) = obj else {
        return (String::new(), false);
    };
    match o.get("type").and_then(Value::as_str) {
        Some("file") => (
            o.get("file")
                .and_then(|f| f.get("url"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            true,
        ),
        Some("external") => (
            o.get("external")
                .and_then(|e| e.get("url"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            false,
        ),
        _ => (String::new(), false),
    }
}

/// Parses a single Notion block JSON object into `NotionBlock`.
pub fn parse_notion_block(
    val: &Value,
    page_id: &str,
    parent_block_id: Option<&str>,
) -> NotionBlock {
    let id = val
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let block_type = val
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unsupported")
        .to_string();
    let has_children = val
        .get("has_children")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let archived = val
        .get("archived")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || val
            .get("in_trash")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    let inner = val.get(&block_type);
    let color = NotionColor::from_notion_str(
        inner
            .and_then(|i| i.get("color"))
            .and_then(Value::as_str),
    );
    let rt = || parse_rich_text_array(inner.and_then(|i| i.get("rich_text")));
    let cap = || parse_rich_text_array(inner.and_then(|i| i.get("caption")));

    let kind = match block_type.as_str() {
        "paragraph" => NotionBlockKind::Paragraph {
            rich_text: rt(),
            color,
        },
        "heading_1" => NotionBlockKind::Heading1 {
            rich_text: rt(),
            is_toggleable: inner
                .and_then(|i| i.get("is_toggleable"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            color,
        },
        "heading_2" => NotionBlockKind::Heading2 {
            rich_text: rt(),
            is_toggleable: inner
                .and_then(|i| i.get("is_toggleable"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            color,
        },
        "heading_3" => NotionBlockKind::Heading3 {
            rich_text: rt(),
            is_toggleable: inner
                .and_then(|i| i.get("is_toggleable"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            color,
        },
        "bulleted_list_item" => NotionBlockKind::BulletedListItem {
            rich_text: rt(),
            color,
        },
        "numbered_list_item" => NotionBlockKind::NumberedListItem {
            rich_text: rt(),
            color,
        },
        "to_do" => NotionBlockKind::ToDo {
            rich_text: rt(),
            checked: inner
                .and_then(|i| i.get("checked"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            color,
        },
        "toggle" => NotionBlockKind::Toggle {
            rich_text: rt(),
            color,
        },
        "callout" => NotionBlockKind::Callout {
            rich_text: rt(),
            icon: parse_icon(inner.and_then(|i| i.get("icon"))),
            color,
        },
        "quote" => NotionBlockKind::Quote {
            rich_text: rt(),
            color,
        },
        "divider" => NotionBlockKind::Divider,
        "code" => NotionBlockKind::Code {
            rich_text: rt(),
            language: inner
                .and_then(|i| i.get("language"))
                .and_then(Value::as_str)
                .unwrap_or("plain text")
                .to_string(),
            caption: cap(),
        },
        "table" => NotionBlockKind::Table {
            table_width: inner
                .and_then(|i| i.get("table_width"))
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize,
            has_column_header: inner
                .and_then(|i| i.get("has_column_header"))
                .and_then(Value::as_bool)
                .unwrap_or(true),
            has_row_header: inner
                .and_then(|i| i.get("has_row_header"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        "table_row" => {
            let cells = inner
                .and_then(|i| i.get("cells"))
                .and_then(Value::as_array)
                .map(|rows| {
                    rows.iter()
                        .map(|cell| parse_rich_text_array(Some(cell)))
                        .collect()
                })
                .unwrap_or_default();
            NotionBlockKind::TableRow { cells }
        }
        "image" => {
            let (url, is_hosted) = extract_media_url(inner);
            NotionBlockKind::Image {
                url,
                caption: cap(),
                is_hosted,
            }
        }
        "video" => {
            let (url, is_hosted) = extract_media_url(inner);
            NotionBlockKind::Video {
                url,
                caption: cap(),
                is_hosted,
            }
        }
        "audio" => {
            let (url, is_hosted) = extract_media_url(inner);
            NotionBlockKind::Audio {
                url,
                caption: cap(),
                is_hosted,
            }
        }
        "file" => {
            let (url, _) = extract_media_url(inner);
            let name = inner
                .and_then(|i| i.get("name"))
                .and_then(Value::as_str)
                .map(str::to_string);
            NotionBlockKind::File {
                url,
                name,
                caption: cap(),
            }
        }
        "pdf" => {
            let (url, _) = extract_media_url(inner);
            NotionBlockKind::Pdf {
                url,
                caption: cap(),
            }
        }
        "bookmark" => NotionBlockKind::Bookmark {
            url: inner
                .and_then(|i| i.get("url"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            caption: cap(),
        },
        "embed" => NotionBlockKind::Embed {
            url: inner
                .and_then(|i| i.get("url"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            caption: cap(),
        },
        "link_preview" => NotionBlockKind::LinkPreview {
            url: inner
                .and_then(|i| i.get("url"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        },
        "child_page" => NotionBlockKind::ChildPage {
            title: inner
                .and_then(|i| i.get("title"))
                .and_then(Value::as_str)
                .unwrap_or("Untitled")
                .to_string(),
        },
        "child_database" => NotionBlockKind::ChildDatabase {
            title: inner
                .and_then(|i| i.get("title"))
                .and_then(Value::as_str)
                .unwrap_or("Untitled Database")
                .to_string(),
        },
        "column_list" => NotionBlockKind::ColumnList,
        "column" => NotionBlockKind::Column,
        "synced_block" => NotionBlockKind::SyncedBlock {
            synced_from_block_id: inner
                .and_then(|i| i.get("synced_from"))
                .and_then(|s| s.get("block_id"))
                .and_then(Value::as_str)
                .map(str::to_string),
        },
        "equation" => NotionBlockKind::Equation {
            expression: inner
                .and_then(|i| i.get("expression"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        },
        "table_of_contents" => NotionBlockKind::TableOfContents { color },
        "button" => {
            let mut rich_text = parse_rich_text_array(
                inner.and_then(|i| i.get("rich_text").or_else(|| i.get("text"))),
            );
            if spans_plain_text(&rich_text).trim().is_empty() {
                if let Some(label) = inner
                    .and_then(|i| i.get("text").or_else(|| i.get("label")))
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                {
                    rich_text = vec![RichTextSpan::plain(label)];
                } else {
                    rich_text = vec![RichTextSpan::plain("Button")];
                }
            }
            NotionBlockKind::Button { rich_text }
        }
        other => NotionBlockKind::Unsupported {
            raw_type: other.to_string(),
        },
    };

    let (parsed_parent_type, parsed_parent_id) = parse_parent(val.get("parent"));
    let resolved_page_id = if !page_id.trim().is_empty() {
        page_id.to_string()
    } else if parsed_parent_type == "page_id" {
        parsed_parent_id.clone().unwrap_or_default()
    } else {
        String::new()
    };
    let resolved_parent_block_id = parent_block_id.map(str::to_string).or_else(|| {
        if parsed_parent_type == "block_id" {
            parsed_parent_id
        } else {
            None
        }
    });

    let children = val
        .get("children")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|c| parse_notion_block(c, &resolved_page_id, Some(&id)))
                .collect()
        })
        .unwrap_or_default();

    NotionBlock {
        id,
        page_id: resolved_page_id,
        parent_block_id: resolved_parent_block_id,
        block_type,
        has_children,
        archived,
        kind,
        children,
    }
}

/// Parses a Notion API `comment` JSON object into `NotionComment`.
pub fn parse_notion_comment(val: &Value, page_id: &str) -> NotionComment {
    let rich_text = parse_rich_text_array(val.get("rich_text"));
    let body = spans_plain_text(&rich_text);
    let author = val
        .get("created_by")
        .and_then(|u| {
            u.get("name")
                .and_then(Value::as_str)
                .or_else(|| u.get("id").and_then(Value::as_str))
        })
        .unwrap_or("Notion User")
        .to_string();
    NotionComment {
        id: val
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        page_id: page_id.to_string(),
        discussion_id: val
            .get("discussion_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        author,
        body,
        rich_text,
        created_time: val
            .get("created_time")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    }
}

/// IDs of blocks that report children but carry none in this tree (document order).
pub fn incomplete_block_ids(blocks: &[NotionBlock]) -> Vec<String> {
    let mut out = Vec::new();
    incomplete_block_ids_rec(blocks, &mut out);
    out
}

fn incomplete_block_ids_rec(blocks: &[NotionBlock], out: &mut Vec<String>) {
    for blk in blocks {
        if blk.has_children
            && blk.children.is_empty()
            && !matches!(
                blk.block_type.as_str(),
                "child_page" | "child_database" | "unsupported"
            )
        {
            out.push(blk.id.clone());
        }
        incomplete_block_ids_rec(&blk.children, out);
    }
}

/// Collects all embedded image/file/pdf media references recursively from a block tree.
pub fn collect_media_refs(blocks: &[NotionBlock]) -> Vec<NotionMediaRef> {
    let mut out = Vec::new();
    collect_media_refs_rec(blocks, &mut out);
    out
}

fn collect_media_refs_rec(blocks: &[NotionBlock], out: &mut Vec<NotionMediaRef>) {
    for blk in blocks {
        match &blk.kind {
            NotionBlockKind::Image { url, caption, .. } if !url.trim().is_empty() => {
                let cap = spans_plain_text(caption);
                let display_name = if !cap.trim().is_empty() {
                    cap
                } else {
                    file_name_from_url(url).unwrap_or_else(|| format!("image-{}.png", out.len() + 1))
                };
                let url_kind = sniff_content_kind(None, Some(url), None);
                let (mime_type, is_image) = match url_kind {
                    ContentKind::Html => ("text/html".to_string(), false),
                    ContentKind::Svg => ("image/svg+xml".to_string(), true),
                    _ => {
                        let mime = guess_mime_from_url(url, true);
                        let safe = crate::img_preview::is_image_mime_or_name(&mime, Some(&display_name));
                        (mime, safe)
                    }
                };
                out.push(NotionMediaRef {
                    block_id: blk.id.clone(),
                    url: url.clone(),
                    display_name,
                    mime_type,
                    is_image,
                });
            }
            NotionBlockKind::File { url, name, caption } if !url.trim().is_empty() => {
                let cap = spans_plain_text(caption);
                let display_name = name
                    .clone()
                    .filter(|s| !s.trim().is_empty())
                    .or_else(|| (!cap.trim().is_empty()).then_some(cap))
                    .or_else(|| file_name_from_url(url))
                    .unwrap_or_else(|| format!("file-{}", out.len() + 1));
                let mime_type = guess_mime_from_url_and_name(url, Some(&display_name), false);
                let is_img = crate::img_preview::is_image_mime_or_name(&mime_type, Some(&display_name));
                out.push(NotionMediaRef {
                    block_id: blk.id.clone(),
                    url: url.clone(),
                    display_name,
                    mime_type,
                    is_image: is_img,
                });
            }
            NotionBlockKind::Video { url, caption, .. } if !url.trim().is_empty() => {
                let cap = spans_plain_text(caption);
                let display_name = if !cap.trim().is_empty() {
                    cap
                } else {
                    file_name_from_url(url).unwrap_or_else(|| format!("video-{}.mp4", out.len() + 1))
                };
                out.push(NotionMediaRef {
                    block_id: blk.id.clone(),
                    display_name,
                    url: url.clone(),
                    mime_type: "video/mp4".to_string(),
                    is_image: false,
                });
            }
            NotionBlockKind::Audio { url, caption, .. } if !url.trim().is_empty() => {
                let cap = spans_plain_text(caption);
                let display_name = if !cap.trim().is_empty() {
                    cap
                } else {
                    file_name_from_url(url).unwrap_or_else(|| format!("audio-{}.mp3", out.len() + 1))
                };
                out.push(NotionMediaRef {
                    block_id: blk.id.clone(),
                    display_name,
                    url: url.clone(),
                    mime_type: "audio/mpeg".to_string(),
                    is_image: false,
                });
            }
            NotionBlockKind::Pdf { url, caption } if !url.trim().is_empty() => {
                let cap = spans_plain_text(caption);
                let display_name = if !cap.trim().is_empty() {
                    cap
                } else {
                    file_name_from_url(url).unwrap_or_else(|| format!("document-{}.pdf", out.len() + 1))
                };
                out.push(NotionMediaRef {
                    block_id: blk.id.clone(),
                    url: url.clone(),
                    display_name,
                    mime_type: "application/pdf".to_string(),
                    is_image: false,
                });
            }
            _ => {}
        }
        collect_media_refs_rec(&blk.children, out);
    }
}

fn file_name_from_url(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let seg = parsed.path_segments()?.next_back()?;
    let decoded = urlencoding::decode(seg).ok()?.into_owned();
    let trimmed = decoded.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn guess_mime_from_url_and_name(url: &str, name_hint: Option<&str>, default_image: bool) -> String {
    let url_mime = guess_mime_from_url(url, false);
    if url_mime != "application/octet-stream" {
        return url_mime;
    }
    if let Some(name) = name_hint {
        let name_mime = guess_mime_from_url(name, false);
        if name_mime != "application/octet-stream" {
            return name_mime;
        }
    }
    if default_image {
        "image/png".to_string()
    } else {
        "application/octet-stream".to_string()
    }
}

fn guess_mime_from_url(url: &str, default_image: bool) -> String {
    let lower = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    if lower.ends_with(".svg") {
        return "image/svg+xml".to_string();
    }
    if lower.ends_with(".xml") {
        return "application/xml".to_string();
    }
    match sniff_content_kind(None, Some(&lower), None) {
        ContentKind::Html => "text/html".to_string(),
        ContentKind::Svg => "image/svg+xml".to_string(),
        ContentKind::Pdf => "application/pdf".to_string(),
        ContentKind::Markdown => "text/markdown".to_string(),
        ContentKind::PlainText => "text/plain".to_string(),
        ContentKind::RasterImage => {
            if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
                "image/jpeg".to_string()
            } else if lower.ends_with(".gif") {
                "image/gif".to_string()
            } else if lower.ends_with(".webp") {
                "image/webp".to_string()
            } else if lower.ends_with(".bmp") {
                "image/bmp".to_string()
            } else {
                "image/png".to_string()
            }
        }
        ContentKind::Binary => {
            if default_image {
                "image/png".to_string()
            } else {
                "application/octet-stream".to_string()
            }
        }
    }
}

/// Flattens a block tree into searchable plain text for SQLite FTS5 indexing.
pub fn blocks_to_plain_text(blocks: &[NotionBlock]) -> String {
    blocks
        .iter()
        .map(NotionBlock::plain_text)
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Renders rich-text spans to inline Markdown (`**bold**`, `*italic*`, `` `code` ``, `~~strike~~`, `[text](url)`).
pub fn rich_text_to_markdown(spans: &[RichTextSpan]) -> String {
    let mut out = String::new();
    for sp in spans {
        let mut chunk = match &sp.kind {
            RichTextKind::Equation { expression } => format!("${expression}$"),
            _ => sp.plain_text.clone(),
        };
        if chunk.is_empty() {
            continue;
        }
        if sp.annotations.code {
            chunk = format!("`{chunk}`");
        }
        if sp.annotations.bold {
            chunk = format!("**{chunk}**");
        }
        if sp.annotations.italic {
            chunk = format!("*{chunk}*");
        }
        if sp.annotations.strikethrough {
            chunk = format!("~~{chunk}~~");
        }
        if let Some(ref href) = sp.href {
            if !href.trim().is_empty() {
                chunk = format!("[{chunk}]({href})");
            }
        }
        out.push_str(&chunk);
    }
    out
}

/// Returns `true` if any block in the tree is a `column_list` or `column` container.
pub fn has_column_blocks(blocks: &[NotionBlock]) -> bool {
    blocks.iter().any(|blk| {
        matches!(
            blk.kind,
            NotionBlockKind::ColumnList | NotionBlockKind::Column
        ) || has_column_blocks(&blk.children)
    })
}

/// Exports a Notion block tree to clean, editable Markdown.
pub fn blocks_to_markdown(blocks: &[NotionBlock]) -> String {
    let mut lines = Vec::new();
    render_blocks_md_with_options(blocks, 0, &mut lines, None, false);
    lines.join("\n")
}

/// Exports a Notion block tree to editor Markdown, adding column-ID boundary
/// comments only when the tree contains column layouts so edits stay in place.
pub fn blocks_to_editor_markdown(blocks: &[NotionBlock]) -> String {
    let mut lines = Vec::new();
    render_blocks_md_with_options(blocks, 0, &mut lines, None, has_column_blocks(blocks));
    lines.join("\n")
}

pub(crate) fn markdown_edit_ranges_with_options(
    blocks: &[NotionBlock],
    include_column_markers: bool,
) -> (String, Vec<std::ops::Range<usize>>) {
    let mut lines = Vec::new();
    let mut ranges = Vec::new();
    render_blocks_md_with_options(blocks, 0, &mut lines, Some(&mut ranges), include_column_markers);
    // A rendered item (such as a code block's content) can contain multiple lines.
    let mut offsets = vec![0];
    for line in &lines {
        offsets.push(offsets.last().unwrap() + line.bytes().filter(|&b| b == b'\n').count() + 1);
    }
    let ranges = ranges.into_iter().map(|r| offsets[r.start]..offsets[r.end]).collect();
    (lines.join("\n"), ranges)
}

fn push_indented_lines(out: &mut Vec<String>, indent: &str, text: &str) {
    for line in text.split('\n') {
        if indent.is_empty() || line.is_empty() {
            out.push(line.to_owned());
        } else {
            out.push(format!("{indent}{line}"));
        }
    }
}

fn render_blocks_md_with_options(
    blocks: &[NotionBlock],
    depth: usize,
    out: &mut Vec<String>,
    mut ranges: Option<&mut Vec<std::ops::Range<usize>>>,
    include_column_markers: bool,
) {
    let indent = "  ".repeat(depth);
    let mut num_counter = 0usize;

    for blk in blocks {
        let start = out.len();
        if !matches!(blk.kind, NotionBlockKind::NumberedListItem { .. }) {
            num_counter = 0;
        }
        match &blk.kind {
            NotionBlockKind::Heading1 { rich_text, .. } => {
                out.push(format!("{indent}# {}", rich_text_to_markdown(rich_text)));
                if !blk.children.is_empty() {
                    render_blocks_md_with_options(
                        &blk.children,
                        depth + 1,
                        out,
                        None,
                        include_column_markers,
                    );
                }
                out.push(String::new());
            }
            NotionBlockKind::Heading2 { rich_text, .. } => {
                out.push(format!("{indent}## {}", rich_text_to_markdown(rich_text)));
                if !blk.children.is_empty() {
                    render_blocks_md_with_options(
                        &blk.children,
                        depth + 1,
                        out,
                        None,
                        include_column_markers,
                    );
                }
                out.push(String::new());
            }
            NotionBlockKind::Heading3 { rich_text, .. } => {
                out.push(format!("{indent}### {}", rich_text_to_markdown(rich_text)));
                if !blk.children.is_empty() {
                    render_blocks_md_with_options(
                        &blk.children,
                        depth + 1,
                        out,
                        None,
                        include_column_markers,
                    );
                }
                out.push(String::new());
            }
            NotionBlockKind::Paragraph { rich_text, .. } => {
                let text = rich_text_to_markdown(rich_text);
                if text.is_empty() {
                    out.push(String::new());
                    if !blk.children.is_empty() {
                        render_blocks_md_with_options(
                            &blk.children,
                            depth + 1,
                            out,
                            None,
                            include_column_markers,
                        );
                    }
                } else {
                    push_indented_lines(out, &indent, &text);
                    if !blk.children.is_empty() {
                        render_blocks_md_with_options(
                            &blk.children,
                            depth + 1,
                            out,
                            None,
                            include_column_markers,
                        );
                    }
                    out.push(String::new());
                }
            }
            NotionBlockKind::BulletedListItem { rich_text, .. } => {
                out.push(format!("{indent}- {}", rich_text_to_markdown(rich_text)));
                if !blk.children.is_empty() {
                    render_blocks_md_with_options(
                        &blk.children,
                        depth + 1,
                        out,
                        None,
                        include_column_markers,
                    );
                }
            }
            NotionBlockKind::NumberedListItem { rich_text, .. } => {
                num_counter += 1;
                out.push(format!(
                    "{indent}{num_counter}. {}",
                    rich_text_to_markdown(rich_text)
                ));
                if !blk.children.is_empty() {
                    render_blocks_md_with_options(
                        &blk.children,
                        depth + 1,
                        out,
                        None,
                        include_column_markers,
                    );
                }
            }
            NotionBlockKind::ToDo {
                rich_text, checked, ..
            } => {
                let box_mark = if *checked { "x" } else { " " };
                out.push(format!(
                    "{indent}- [{box_mark}] {}",
                    rich_text_to_markdown(rich_text)
                ));
                if !blk.children.is_empty() {
                    render_blocks_md_with_options(
                        &blk.children,
                        depth + 1,
                        out,
                        None,
                        include_column_markers,
                    );
                }
            }
            NotionBlockKind::Toggle { rich_text, .. } => {
                out.push(format!("{indent}> ▸ {}", rich_text_to_markdown(rich_text)));
                if !blk.children.is_empty() {
                    render_blocks_md_with_options(
                        &blk.children,
                        depth + 1,
                        out,
                        None,
                        include_column_markers,
                    );
                }
                out.push(String::new());
            }
            NotionBlockKind::Callout {
                rich_text, icon, ..
            } => {
                let ic = icon.as_deref().unwrap_or("💡");
                out.push(format!("{indent}> {ic} {}", rich_text_to_markdown(rich_text)));
                if !blk.children.is_empty() {
                    render_blocks_md_with_options(
                        &blk.children,
                        depth + 1,
                        out,
                        None,
                        include_column_markers,
                    );
                }
                out.push(String::new());
            }
            NotionBlockKind::Quote { rich_text, .. } => {
                out.push(format!("{indent}> {}", rich_text_to_markdown(rich_text)));
                if !blk.children.is_empty() {
                    render_blocks_md_with_options(
                        &blk.children,
                        depth + 1,
                        out,
                        None,
                        include_column_markers,
                    );
                }
                out.push(String::new());
            }
            NotionBlockKind::Divider => {
                out.push(format!("{indent}---"));
                out.push(String::new());
            }
            NotionBlockKind::Code {
                rich_text,
                language,
                ..
            } => {
                let lang = if language == "plain text" {
                    ""
                } else {
                    language.as_str()
                };
                out.push(format!("{indent}```{lang}"));
                push_indented_lines(out, &indent, &spans_plain_text(rich_text));
                out.push(format!("{indent}```"));
                out.push(String::new());
            }
            NotionBlockKind::Table { .. } => {
                for (row_idx, row) in blk.children.iter().enumerate() {
                    if let NotionBlockKind::TableRow { cells } = &row.kind {
                        let cols: Vec<String> = cells
                            .iter()
                            .map(|c| escape_table_cell(&rich_text_to_markdown(c)))
                            .collect();
                        out.push(format!("{indent}| {} |", cols.join(" | ")));
                        if row_idx == 0 {
                            let sep: Vec<&str> = vec!["---"; cols.len().max(1)];
                            out.push(format!("{indent}| {} |", sep.join(" | ")));
                        }
                    }
                }
                out.push(String::new());
            }
            NotionBlockKind::TableRow { cells } => {
                let cols: Vec<String> = cells
                    .iter()
                    .map(|c| escape_table_cell(&rich_text_to_markdown(c)))
                    .collect();
                out.push(format!("{indent}| {} |", cols.join(" | ")));
            }
            NotionBlockKind::Image { url, caption, .. } => {
                let trimmed_url = url.trim();
                let alt = spans_plain_text(caption);
                if !trimmed_url.is_empty() {
                    out.push(format!("{indent}![{alt}]({trimmed_url})"));
                    out.push(String::new());
                } else if !alt.trim().is_empty() {
                    out.push(format!("{indent}[🖼 {}]", alt.trim()));
                    out.push(String::new());
                }
            }
            NotionBlockKind::File { url, name, .. } => {
                let label = name.as_deref().unwrap_or("File");
                out.push(format!("{indent}[📎 {label}]({url})"));
                out.push(String::new());
            }
            NotionBlockKind::Video { url, caption, .. } => {
                let label = spans_plain_text(caption);
                let label = if label.is_empty() { "Video" } else { &label };
                out.push(format!("{indent}[🎬 {label}]({url})"));
                out.push(String::new());
            }
            NotionBlockKind::Audio { url, caption, .. } => {
                let label = spans_plain_text(caption);
                let label = if label.is_empty() { "Audio" } else { &label };
                out.push(format!("{indent}[🎵 {label}]({url})"));
                out.push(String::new());
            }
            NotionBlockKind::Pdf { url, caption } => {
                let label = spans_plain_text(caption);
                let label = if label.is_empty() { "PDF" } else { &label };
                out.push(format!("{indent}[📄 {label}]({url})"));
                out.push(String::new());
            }
            NotionBlockKind::Bookmark { url, caption }
            | NotionBlockKind::Embed { url, caption } => {
                let label = spans_plain_text(caption);
                if label.is_empty() {
                    out.push(format!("{indent}[{url}]({url})"));
                } else {
                    out.push(format!("{indent}[{label}]({url})"));
                }
                out.push(String::new());
            }
            NotionBlockKind::LinkPreview { url } => {
                out.push(format!("{indent}[{url}]({url})"));
                out.push(String::new());
            }
            NotionBlockKind::ChildPage { title } => {
                out.push(format!("{indent}📄 **{title}**"));
                out.push(String::new());
            }
            NotionBlockKind::ChildDatabase { title } => {
                out.push(format!("{indent}🗂 **{title}**"));
                out.push(String::new());
            }
            NotionBlockKind::Equation { expression } => {
                out.push(format!("{indent}$$\n{expression}\n$$"));
                out.push(String::new());
            }
            NotionBlockKind::ColumnList => {
                if include_column_markers {
                    out.push(format!("{indent}<!-- notion:column-list id={} -->", blk.id));
                    for col in &blk.children {
                        if matches!(col.kind, NotionBlockKind::Column) {
                            out.push(format!("{indent}<!-- notion:column id={} -->", col.id));
                            if col.children.is_empty() {
                                out.push(String::new());
                            } else {
                                render_blocks_md_with_options(
                                    &col.children,
                                    depth,
                                    out,
                                    None,
                                    include_column_markers,
                                );
                            }
                        } else {
                            render_blocks_md_with_options(
                                std::slice::from_ref(col),
                                depth,
                                out,
                                None,
                                include_column_markers,
                            );
                        }
                    }
                    out.push(format!("{indent}<!-- /notion:column-list -->"));
                    out.push(String::new());
                } else {
                    render_blocks_md_with_options(&blk.children, depth, out, None, false);
                }
            }
            NotionBlockKind::Column | NotionBlockKind::SyncedBlock { .. } => {
                render_blocks_md_with_options(
                    &blk.children,
                    depth,
                    out,
                    None,
                    include_column_markers,
                );
            }
            NotionBlockKind::TableOfContents { .. } => {
                out.push(format!("{indent}[Table of Contents]"));
                out.push(String::new());
            }
            NotionBlockKind::Button { rich_text } => {
                let label = spans_plain_text(rich_text);
                let label = if label.trim().is_empty() {
                    "Button"
                } else {
                    label.trim()
                };
                out.push(format!("{indent}[⚡ {label}]"));
                out.push(String::new());
            }
            NotionBlockKind::Unsupported { raw_type } => {
                out.push(format!("{indent}[unsupported: {raw_type}]"));
            }
        }
        if let Some(ranges) = ranges.as_mut() {
            ranges.push(start..out.len());
        }
    }
}

pub(crate) fn block_inline_markdown<'a>(block_type: &str, line: &'a str) -> &'a str {
    let line = line.trim();
    match block_type {
        "heading_1" | "heading_2" | "heading_3" => line.split_once(' ').map_or(line, |(_, s)| s),
        "to_do" => line.get(6..).unwrap_or_default(),
        "bulleted_list_item" => line.get(2..).unwrap_or_default(),
        "numbered_list_item" => strip_numbered_prefix(line).unwrap_or(line),
        "toggle" => line.strip_prefix("> ▸ ").unwrap_or(line),
        "callout" => line.strip_prefix("> ").and_then(split_leading_emoji).map_or(line, |(_, s)| s),
        "quote" => line.strip_prefix("> ").unwrap_or(line),
        _ => line,
    }
    .trim()
}

/// Converts inline Markdown (`**bold**`, `*italic*`, `` `code` ``, `~~strike~~`, `[text](url)`)
/// into a Notion API `rich_text` JSON array.
pub fn parse_inline_markdown_rich_text(input: &str) -> Vec<Value> {
    inline_markdown_spans(input).into_iter().map(|(_, span)| span).collect()
}

/// Source ranges use Unicode scalar offsets, matching the editor's inline diff.
pub(crate) fn inline_markdown_spans(input: &str) -> Vec<(std::ops::Range<usize>, Value)> {
    let mut spans = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let len = chars.len();
    let mut i = 0usize;
    let mut plain_buf = String::new();

    let flush_plain = |buf: &mut String, end: usize, out: &mut Vec<_>| {
        if !buf.is_empty() {
            let start = end - buf.chars().count();
            out.push((start..end, json!({
                "type": "text",
                "text": { "content": std::mem::take(buf) }
            })));
        }
    };

    while i < len {
        // Inline link: [label](url)
        if chars[i] == '[' {
            if let Some(close_br) = (i + 1..len).find(|&j| chars[j] == ']') {
                if close_br + 1 < len && chars[close_br + 1] == '(' {
                    if let Some(close_par) = (close_br + 2..len).find(|&j| chars[j] == ')') {
                        let label: String = chars[i + 1..close_br].iter().collect();
                        let href: String = chars[close_br + 2..close_par].iter().collect();
                        if href.starts_with("http://") || href.starts_with("https://") {
                            flush_plain(&mut plain_buf, i, &mut spans);
                            spans.push((i..close_par + 1, json!({
                                "type": "text",
                                "text": { "content": label, "link": { "url": href } }
                            })));
                            i = close_par + 1;
                            continue;
                        }
                    }
                }
            }
        }

        // Inline code: `code`
        if chars[i] == '`' {
            if let Some(end) = (i + 1..len).find(|&j| chars[j] == '`') {
                let code: String = chars[i + 1..end].iter().collect();
                flush_plain(&mut plain_buf, i, &mut spans);
                spans.push((i..end + 1, json!({
                    "type": "text",
                    "text": { "content": code },
                    "annotations": { "code": true }
                })));
                i = end + 1;
                continue;
            }
        }

        // Bold: **text**
        if i + 1 < len && chars[i] == '*' && chars[i + 1] == '*' {
            if let Some(end) = (i + 2..len.saturating_sub(1))
                .find(|&j| chars[j] == '*' && chars[j + 1] == '*')
            {
                let text: String = chars[i + 2..end].iter().collect();
                flush_plain(&mut plain_buf, i, &mut spans);
                spans.push((i..end + 2, json!({
                    "type": "text",
                    "text": { "content": text },
                    "annotations": { "bold": true }
                })));
                i = end + 2;
                continue;
            }
        }

        // Strikethrough: ~~text~~
        if i + 1 < len && chars[i] == '~' && chars[i + 1] == '~' {
            if let Some(end) = (i + 2..len.saturating_sub(1))
                .find(|&j| chars[j] == '~' && chars[j + 1] == '~')
            {
                let text: String = chars[i + 2..end].iter().collect();
                flush_plain(&mut plain_buf, i, &mut spans);
                spans.push((i..end + 2, json!({
                    "type": "text",
                    "text": { "content": text },
                    "annotations": { "strikethrough": true }
                })));
                i = end + 2;
                continue;
            }
        }

        // Italic: *text*
        if chars[i] == '*' {
            if let Some(end) = (i + 1..len).find(|&j| chars[j] == '*') {
                if end > i + 1 {
                    let text: String = chars[i + 1..end].iter().collect();
                    flush_plain(&mut plain_buf, i, &mut spans);
                    spans.push((i..end + 1, json!({
                        "type": "text",
                        "text": { "content": text },
                        "annotations": { "italic": true }
                    })));
                    i = end + 1;
                    continue;
                }
            }
        }

        plain_buf.push(chars[i]);
        i += 1;
    }

    flush_plain(&mut plain_buf, i, &mut spans);
    if spans.is_empty() {
        spans.push((0..0, json!({ "type": "text", "text": { "content": "" } })));
    }
    spans
}

fn leading_indent_level(raw: &str) -> usize {
    let mut spaces = 0usize;
    for ch in raw.chars() {
        match ch {
            ' ' => spaces += 1,
            '\t' => spaces += 2,
            _ => break,
        }
    }
    spaces / 2
}

/// Keeps a cell on one table row: `|` would start a new cell and a newline a new row.
fn escape_table_cell(cell: &str) -> String {
    cell.replace('|', "\\|").replace("\r\n", "<br>").replace('\n', "<br>")
}

fn split_markdown_table_row(line: &str) -> Vec<String> {
    let inner = line.trim();
    let inner = inner.strip_prefix('|').unwrap_or(inner);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut escaped = false;
    for ch in inner.chars() {
        if escaped {
            cur.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '|' {
            cells.push(cur.trim().to_string());
            cur.clear();
        } else {
            cur.push(ch);
        }
    }
    if escaped {
        cur.push('\\');
    }
    cells.push(cur.trim().to_string());
    cells
}

pub(crate) fn is_markdown_table_separator(line: &str) -> bool {
    let trimmed = line.trim();
    if !trimmed.starts_with('|') {
        return false;
    }
    let cells = split_markdown_table_row(trimmed);
    !cells.is_empty()
        && cells.iter().all(|cell| {
            let t = cell.trim();
            !t.is_empty()
                && t.chars().all(|ch| ch == '-' || ch == ':')
                && t.contains('-')
        })
}

fn build_table_row_block(cells: &[String], width: usize) -> Value {
    let mut notion_cells = Vec::with_capacity(width);
    for idx in 0..width {
        let cell_text = cells.get(idx).map(String::as_str).unwrap_or("");
        notion_cells.push(Value::Array(parse_inline_markdown_rich_text(cell_text)));
    }
    json!({
        "object": "block",
        "type": "table_row",
        "table_row": {
            "cells": notion_cells
        }
    })
}

fn push_list_block(
    blocks: &mut Vec<Value>,
    active_list_root: &mut Option<(usize, usize)>,
    indent: usize,
    item: Value,
) {
    // Notion's Append block children endpoint accepts at most 2 levels of nesting per request
    // (top-level block + direct `children`). Items deeper than the active root are attached as
    // children of that root so deeper levels stay siblings at the deepest permitted level.
    if let Some((root_idx, root_indent)) = *active_list_root {
        if indent > root_indent {
            if let Some(root) = blocks.get_mut(root_idx) {
                if let Some(root_type) = root
                    .get("type")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                {
                    if let Some(obj) = root.get_mut(&root_type).and_then(Value::as_object_mut) {
                        let entry = obj
                            .entry("children".to_string())
                            .or_insert_with(|| Value::Array(Vec::new()));
                        if let Some(arr) = entry.as_array_mut() {
                            arr.push(item);
                            return;
                        }
                    }
                }
            }
        }
    }
    blocks.push(item);
    *active_list_root = Some((blocks.len() - 1, indent));
}

/// Parses Markdown text into Notion REST API v1 block payloads (`heading_1/2/3`, `to_do`,
/// `bulleted_list_item`, `numbered_list_item`, `table`, `code`, `quote`, `callout`, `toggle`, `divider`, `paragraph`).
pub fn markdown_to_notion_blocks(markdown: &str) -> Vec<Value> {
    let mut blocks = Vec::new();
    let lines: Vec<&str> = markdown.lines().collect();
    let mut i = 0usize;
    let mut active_list_root: Option<(usize, usize)> = None;

    while i < lines.len() {
        let raw = lines[i];
        let indent = leading_indent_level(raw);
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            i += 1;
            continue;
        }

        // Fenced code block ```lang
        if let Some(lang_raw) = trimmed.strip_prefix("```") {
            active_list_root = None;
            let language = normalize_notion_code_lang(lang_raw.trim());
            i += 1;
            let mut code_lines = Vec::new();
            while i < lines.len() && !lines[i].trim().starts_with("```") {
                code_lines.push(lines[i]);
                i += 1;
            }
            if i < lines.len() {
                i += 1;
            }
            blocks.push(json!({
                "object": "block",
                "type": "code",
                "code": {
                    "rich_text": [{ "type": "text", "text": { "content": code_lines.join("\n") } }],
                    "language": language
                }
            }));
            continue;
        }

        // GFM Markdown table: header row starting with `|` followed by separator row `| --- | --- |`
        if trimmed.starts_with('|')
            && i + 1 < lines.len()
            && is_markdown_table_separator(lines[i + 1])
        {
            active_list_root = None;
            let header_cells = split_markdown_table_row(trimmed);
            let width = header_cells.len().max(1);
            let mut rows = vec![build_table_row_block(&header_cells, width)];
            i += 2;
            while i < lines.len() && lines[i].trim().starts_with('|') {
                let row_cells = split_markdown_table_row(lines[i]);
                rows.push(build_table_row_block(&row_cells, width));
                i += 1;
            }
            blocks.push(json!({
                "object": "block",
                "type": "table",
                "table": {
                    "table_width": width,
                    "has_column_header": true,
                    "has_row_header": false,
                    "children": rows
                }
            }));
            continue;
        }

        // Divider --- or ***
        if trimmed == "---" || trimmed == "***" || trimmed == "___" {
            active_list_root = None;
            blocks.push(json!({
                "object": "block",
                "type": "divider",
                "divider": {}
            }));
            i += 1;
            continue;
        }

        // Headings
        if let Some(rest) = trimmed.strip_prefix("### ") {
            active_list_root = None;
            blocks.push(json!({
                "object": "block",
                "type": "heading_3",
                "heading_3": { "rich_text": parse_inline_markdown_rich_text(rest.trim()) }
            }));
            i += 1;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("## ") {
            active_list_root = None;
            blocks.push(json!({
                "object": "block",
                "type": "heading_2",
                "heading_2": { "rich_text": parse_inline_markdown_rich_text(rest.trim()) }
            }));
            i += 1;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("# ") {
            active_list_root = None;
            blocks.push(json!({
                "object": "block",
                "type": "heading_1",
                "heading_1": { "rich_text": parse_inline_markdown_rich_text(rest.trim()) }
            }));
            i += 1;
            continue;
        }

        // To-do checkboxes: - [ ] or - [x]
        if let Some(rest) = trimmed
            .strip_prefix("- [ ] ")
            .or_else(|| trimmed.strip_prefix("* [ ] "))
        {
            push_list_block(
                &mut blocks,
                &mut active_list_root,
                indent,
                json!({
                    "object": "block",
                    "type": "to_do",
                    "to_do": {
                        "rich_text": parse_inline_markdown_rich_text(rest.trim()),
                        "checked": false
                    }
                }),
            );
            i += 1;
            continue;
        }
        if let Some(rest) = trimmed
            .strip_prefix("- [x] ")
            .or_else(|| trimmed.strip_prefix("- [X] "))
            .or_else(|| trimmed.strip_prefix("* [x] "))
            .or_else(|| trimmed.strip_prefix("* [X] "))
        {
            push_list_block(
                &mut blocks,
                &mut active_list_root,
                indent,
                json!({
                    "object": "block",
                    "type": "to_do",
                    "to_do": {
                        "rich_text": parse_inline_markdown_rich_text(rest.trim()),
                        "checked": true
                    }
                }),
            );
            i += 1;
            continue;
        }

        // Bulleted list
        if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            push_list_block(
                &mut blocks,
                &mut active_list_root,
                indent,
                json!({
                    "object": "block",
                    "type": "bulleted_list_item",
                    "bulleted_list_item": {
                        "rich_text": parse_inline_markdown_rich_text(rest.trim())
                    }
                }),
            );
            i += 1;
            continue;
        }

        // Numbered list: 1. text
        if let Some(rest) = strip_numbered_prefix(trimmed) {
            push_list_block(
                &mut blocks,
                &mut active_list_root,
                indent,
                json!({
                    "object": "block",
                    "type": "numbered_list_item",
                    "numbered_list_item": {
                        "rich_text": parse_inline_markdown_rich_text(rest.trim())
                    }
                }),
            );
            i += 1;
            continue;
        }

        // Quote / Callout / Toggle: `> 💡 text`, `> ▸ toggle`, `> quote`
        if let Some(rest) = trimmed.strip_prefix("> ") {
            active_list_root = None;
            let inner = rest.trim();
            if let Some(toggle_text) = inner.strip_prefix("▸ ") {
                blocks.push(json!({
                    "object": "block",
                    "type": "toggle",
                    "toggle": {
                        "rich_text": parse_inline_markdown_rich_text(toggle_text.trim())
                    }
                }));
            } else if let Some((emoji, body)) = split_leading_emoji(inner) {
                blocks.push(json!({
                    "object": "block",
                    "type": "callout",
                    "callout": {
                        "rich_text": parse_inline_markdown_rich_text(body.trim()),
                        "icon": { "type": "emoji", "emoji": emoji }
                    }
                }));
            } else {
                blocks.push(json!({
                    "object": "block",
                    "type": "quote",
                    "quote": {
                        "rich_text": parse_inline_markdown_rich_text(inner)
                    }
                }));
            }
            i += 1;
            continue;
        }

        // Image: ![alt](https://...) — never emit an image block with an empty or invalid external URL
        if trimmed.starts_with("![") && trimmed.ends_with(')') {
            if let Some(mid) = trimmed.find("](") {
                let url = trimmed[mid + 2..trimmed.len() - 1].trim();
                if !url.is_empty()
                    && (url.starts_with("http://") || url.starts_with("https://"))
                    && url::Url::parse(url).is_ok()
                {
                    active_list_root = None;
                    let alt = &trimmed[2..mid];
                    let caption = if alt.is_empty() {
                        vec![]
                    } else {
                        vec![json!({ "type": "text", "text": { "content": alt } })]
                    };
                    blocks.push(json!({
                        "object": "block",
                        "type": "image",
                        "image": {
                            "type": "external",
                            "external": { "url": url },
                            "caption": caption
                        }
                    }));
                    i += 1;
                    continue;
                }
            }
        }

        // Default paragraph
        active_list_root = None;
        blocks.push(json!({
            "object": "block",
            "type": "paragraph",
            "paragraph": {
                "rich_text": parse_inline_markdown_rich_text(trimmed)
            }
        }));
        i += 1;
    }

    blocks
}

/// Parses Notion slash-style Quick Append input (`/h1`, `/h2`, `/h3`, `/todo`, `/callout`,
/// `/code`, `/quote`, `/toggle`, `/bullet`, `/num`, `/divider`, or plain Markdown).
pub fn parse_slash_command_blocks(input: &str) -> Vec<Value> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    if let Some(rest) = trimmed.strip_prefix("/h1 ") {
        return vec![json!({
            "object": "block",
            "type": "heading_1",
            "heading_1": { "rich_text": parse_inline_markdown_rich_text(rest.trim()) }
        })];
    }
    if let Some(rest) = trimmed.strip_prefix("/h2 ") {
        return vec![json!({
            "object": "block",
            "type": "heading_2",
            "heading_2": { "rich_text": parse_inline_markdown_rich_text(rest.trim()) }
        })];
    }
    if let Some(rest) = trimmed.strip_prefix("/h3 ") {
        return vec![json!({
            "object": "block",
            "type": "heading_3",
            "heading_3": { "rich_text": parse_inline_markdown_rich_text(rest.trim()) }
        })];
    }
    if let Some(rest) = trimmed.strip_prefix("/todo ") {
        return vec![json!({
            "object": "block",
            "type": "to_do",
            "to_do": {
                "rich_text": parse_inline_markdown_rich_text(rest.trim()),
                "checked": false
            }
        })];
    }
    if let Some(rest) = trimmed.strip_prefix("/callout ") {
        let (icon, body) = split_leading_emoji(rest.trim()).unwrap_or(("💡".to_string(), rest.trim()));
        return vec![json!({
            "object": "block",
            "type": "callout",
            "callout": {
                "rich_text": parse_inline_markdown_rich_text(body.trim()),
                "icon": { "type": "emoji", "emoji": icon }
            }
        })];
    }
    if let Some(rest) = trimmed.strip_prefix("/quote ") {
        return vec![json!({
            "object": "block",
            "type": "quote",
            "quote": { "rich_text": parse_inline_markdown_rich_text(rest.trim()) }
        })];
    }
    if let Some(rest) = trimmed.strip_prefix("/toggle ") {
        return vec![json!({
            "object": "block",
            "type": "toggle",
            "toggle": { "rich_text": parse_inline_markdown_rich_text(rest.trim()) }
        })];
    }
    if let Some(rest) = trimmed.strip_prefix("/bullet ") {
        return vec![json!({
            "object": "block",
            "type": "bulleted_list_item",
            "bulleted_list_item": { "rich_text": parse_inline_markdown_rich_text(rest.trim()) }
        })];
    }
    if let Some(rest) = trimmed.strip_prefix("/num ") {
        return vec![json!({
            "object": "block",
            "type": "numbered_list_item",
            "numbered_list_item": { "rich_text": parse_inline_markdown_rich_text(rest.trim()) }
        })];
    }
    if trimmed == "/divider" || trimmed == "/hr" {
        return vec![json!({
            "object": "block",
            "type": "divider",
            "divider": {}
        })];
    }
    if let Some(rest) = trimmed.strip_prefix("/code") {
        let body = rest.trim_start();
        let (lang, code) = if let Some((first, after)) = body.split_once('\n') {
            (normalize_notion_code_lang(first.trim()), after.to_string())
        } else if let Some((first, after)) = body.split_once(' ') {
            if is_known_code_lang(first) {
                (normalize_notion_code_lang(first), after.to_string())
            } else {
                ("plain text".to_string(), body.to_string())
            }
        } else {
            ("plain text".to_string(), body.to_string())
        };
        return vec![json!({
            "object": "block",
            "type": "code",
            "code": {
                "rich_text": [{ "type": "text", "text": { "content": code } }],
                "language": lang
            }
        })];
    }

    markdown_to_notion_blocks(trimmed)
}

pub(crate) fn strip_numbered_prefix(s: &str) -> Option<&str> {
    let dot_idx = s.find(". ")?;
    if dot_idx == 0 || dot_idx > 4 {
        return None;
    }
    if s[..dot_idx].chars().all(|c| c.is_ascii_digit()) {
        Some(&s[dot_idx + 2..])
    } else {
        None
    }
}

fn is_emoji_scalar(ch: char) -> bool {
    let cp = ch as u32;
    (0x1F300..=0x1FAFF).contains(&cp)
        || (0x2600..=0x27BF).contains(&cp)
        || (0x1F000..=0x1F02F).contains(&cp)
        || (0x1F0A0..=0x1F0FF).contains(&cp)
        || matches!(
            cp,
            0x203C
                | 0x2049
                | 0x2122
                | 0x2139
                | 0x2194..=0x21AA
                | 0x231A..=0x231B
                | 0x2328
                | 0x23CF
                | 0x23E9..=0x23F3
                | 0x23F8..=0x23FA
                | 0x24C2
                | 0x25AA..=0x25AB
                | 0x25B6
                | 0x25C0
                | 0x25FB..=0x25FE
                | 0x2934..=0x2935
                | 0x2B05..=0x2B07
                | 0x2B1B..=0x2B1C
                | 0x2B50
                | 0x2B55
                | 0x3030
                | 0x303D
                | 0x3297
                | 0x3299
                | 0x1F170..=0x1F171
                | 0x1F17E..=0x1F17F
                | 0x1F18E
                | 0x1F191..=0x1F19A
                | 0x1F201..=0x1F202
                | 0x1F21A
                | 0x1F22F
                | 0x1F232..=0x1F23A
                | 0x1F250..=0x1F251
        )
}

fn is_regional_indicator(ch: char) -> bool {
    (0x1F1E6..=0x1F1FF).contains(&(ch as u32))
}

fn is_emoji_grapheme(grapheme: &str) -> bool {
    let mut chars = grapheme.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if is_regional_indicator(first) {
        return chars.next().is_some_and(is_regional_indicator);
    }
    if is_emoji_scalar(first) {
        return true;
    }
    if matches!(first, '0'..='9' | '#' | '*') && grapheme.contains('\u{20E3}') {
        return true;
    }
    false
}

pub(crate) fn split_leading_emoji(s: &str) -> Option<(String, &str)> {
    let grapheme = s.graphemes(true).next()?;
    if !is_emoji_grapheme(grapheme) {
        return None;
    }
    let rest = s[grapheme.len()..].trim_start();
    Some((grapheme.to_string(), rest))
}

fn is_known_code_lang(lang: &str) -> bool {
    matches!(
        lang.to_ascii_lowercase().as_str(),
        "rust"
            | "rs"
            | "python"
            | "py"
            | "javascript"
            | "js"
            | "typescript"
            | "ts"
            | "bash"
            | "sh"
            | "shell"
            | "json"
            | "yaml"
            | "yml"
            | "toml"
            | "sql"
            | "go"
            | "java"
            | "c"
            | "cpp"
            | "html"
            | "css"
            | "markdown"
            | "md"
            | "diff"
            | "powershell"
    )
}

fn normalize_notion_code_lang(lang: &str) -> String {
    match lang.trim().to_ascii_lowercase().as_str() {
        "" | "text" | "txt" | "plain" => "plain text".to_string(),
        "rs" => "rust".to_string(),
        "py" => "python".to_string(),
        "js" => "javascript".to_string(),
        "ts" => "typescript".to_string(),
        "sh" | "zsh" => "shell".to_string(),
        "yml" => "yaml".to_string(),
        "md" => "markdown".to_string(),
        other => other.to_string(),
    }
}

/// Produces a normalized `(block_type, comparable_content)` key for diff-syncing blocks
/// between existing Notion API blocks and newly parsed Markdown payloads.
pub fn block_comparison_key(blk: &NotionBlock) -> (String, String) {
    let content = match &blk.kind {
        NotionBlockKind::Paragraph { rich_text, .. }
        | NotionBlockKind::Heading1 { rich_text, .. }
        | NotionBlockKind::Heading2 { rich_text, .. }
        | NotionBlockKind::Heading3 { rich_text, .. }
        | NotionBlockKind::BulletedListItem { rich_text, .. }
        | NotionBlockKind::NumberedListItem { rich_text, .. }
        | NotionBlockKind::Toggle { rich_text, .. }
        | NotionBlockKind::Quote { rich_text, .. } => rich_text_to_markdown(rich_text),
        NotionBlockKind::ToDo {
            rich_text, checked, ..
        } => format!(
            "{}|{}",
            if *checked { "1" } else { "0" },
            rich_text_to_markdown(rich_text)
        ),
        NotionBlockKind::Callout {
            rich_text, icon, ..
        } => format!(
            "{}|{}",
            icon.as_deref().unwrap_or("💡"),
            rich_text_to_markdown(rich_text)
        ),
        NotionBlockKind::Code {
            rich_text,
            language,
            ..
        } => format!(
            "{}|{}",
            normalize_notion_code_lang(language),
            spans_plain_text(rich_text)
        ),
        NotionBlockKind::Divider => String::new(),
        NotionBlockKind::Image { url, caption, .. } => {
            format!("{}|{}", url.trim(), spans_plain_text(caption))
        }
        NotionBlockKind::ChildPage { title } | NotionBlockKind::ChildDatabase { title } => {
            title.clone()
        }
        _ => blk.plain_text(),
    };
    (blk.block_type.clone(), content)
}

/// Returns `true` for hosted media and structural Notion blocks that must NEVER
/// be deleted or recreated during Markdown diff-sync or optimistic local preview updates.
pub fn is_preserved_anchor_block(blk: &NotionBlock) -> bool {
    match &blk.kind {
        NotionBlockKind::ChildPage { .. }
        | NotionBlockKind::ChildDatabase { .. }
        | NotionBlockKind::File { .. }
        | NotionBlockKind::Pdf { .. }
        | NotionBlockKind::Video { .. }
        | NotionBlockKind::Audio { .. }
        | NotionBlockKind::Bookmark { .. }
        | NotionBlockKind::Embed { .. }
        | NotionBlockKind::Table { .. }
        | NotionBlockKind::ColumnList
        | NotionBlockKind::SyncedBlock { .. } => true,
        NotionBlockKind::Image { url, is_hosted, .. } => *is_hosted || url.trim().is_empty(),
        _ => false,
    }
}

fn find_key_subslice(
    haystack: &[(String, String)],
    needle: &[(String, String)],
    start: usize,
) -> Option<usize> {
    if needle.is_empty() || start + needle.len() > haystack.len() {
        return None;
    }
    (start..=haystack.len() - needle.len())
        .find(|&pos| haystack[pos..pos + needle.len()] == *needle)
}

fn image_url_base(url: &str) -> &str {
    url.trim().split(['?', '#']).next().unwrap_or("")
}

/// Partitions `existing` around `is_preserved_anchor_block` blocks into `k + 1` editable segments
/// and aligns `new_parsed` into `k + 1` corresponding index ranges, consuming any markdown
/// representations of preserved anchors so they are neither duplicated nor deleted.
pub fn partition_blocks_around_anchors(
    existing: &[NotionBlock],
    new_parsed: &[NotionBlock],
) -> (
    Vec<Vec<NotionBlock>>,
    Vec<NotionBlock>,
    Vec<std::ops::Range<usize>>,
) {
    let mut old_segments: Vec<Vec<NotionBlock>> = vec![Vec::new()];
    let mut anchors: Vec<NotionBlock> = Vec::new();
    for blk in existing {
        if is_preserved_anchor_block(blk) {
            anchors.push(blk.clone());
            old_segments.push(Vec::new());
        } else {
            old_segments.last_mut().unwrap().push(blk.clone());
        }
    }

    if anchors.is_empty() {
        return (old_segments, anchors, vec![0..new_parsed.len()]);
    }

    let new_keys: Vec<(String, String)> = new_parsed.iter().map(block_comparison_key).collect();
    let mut new_ranges: Vec<std::ops::Range<usize>> = Vec::with_capacity(old_segments.len());
    let mut cursor = 0usize;

    for (j, anchor) in anchors.iter().enumerate() {
        let anchor_md = blocks_to_markdown(std::slice::from_ref(anchor));
        let anchor_keys: Vec<(String, String)> = markdown_to_notion_blocks(&anchor_md)
            .iter()
            .map(|val| parse_notion_block(val, &anchor.page_id, None))
            .map(|blk| block_comparison_key(&blk))
            .collect();

        let matched = if !anchor_keys.is_empty() {
            find_key_subslice(&new_keys, &anchor_keys, cursor)
                .map(|pos| (pos, anchor_keys.len()))
                .or_else(|| {
                    if let NotionBlockKind::Image { url, .. } = &anchor.kind {
                        let base = image_url_base(url);
                        if !base.is_empty() {
                            return (cursor..new_parsed.len()).find_map(|pos| {
                                if let NotionBlockKind::Image { url: new_url, .. } =
                                    &new_parsed[pos].kind
                                {
                                    (image_url_base(new_url) == base).then_some((pos, 1))
                                } else {
                                    None
                                }
                            });
                        }
                    }
                    None
                })
        } else {
            None
        };

        if let Some((match_pos, match_len)) = matched {
            new_ranges.push(cursor..match_pos);
            cursor = match_pos + match_len;
        } else {
            let split = if old_segments[j + 1..].iter().all(|seg| seg.is_empty()) {
                new_keys.len()
            } else {
                let next_non_empty = old_segments[j + 1..].iter().find(|seg| !seg.is_empty());
                let next_match = next_non_empty.and_then(|seg| {
                    let next_keys: Vec<(String, String)> =
                        seg.iter().map(block_comparison_key).collect();
                    find_key_subslice(&new_keys, &next_keys, cursor).or_else(|| {
                        next_keys
                            .first()
                            .and_then(|k| find_key_subslice(&new_keys, std::slice::from_ref(k), cursor))
                    })
                });
                if let Some(pos) = next_match {
                    pos
                } else if !old_segments[j].is_empty() {
                    let cur_keys: Vec<(String, String)> =
                        old_segments[j].iter().map(block_comparison_key).collect();
                    let mut p = 0usize;
                    while p < cur_keys.len()
                        && cursor + p < new_keys.len()
                        && cur_keys[p] == new_keys[cursor + p]
                    {
                        p += 1;
                    }
                    if p > 0 {
                        cursor + p
                    } else {
                        (cursor + old_segments[j].len()).min(new_keys.len())
                    }
                } else {
                    cursor
                }
            };
            new_ranges.push(cursor..split);
            cursor = split;
        }
    }
    new_ranges.push(cursor..new_keys.len());

    (old_segments, anchors, new_ranges)
}

/// Immediately parses `edited_markdown` into typed `NotionBlock`s (preserving existing
/// hosted media, `ChildPage`, `ChildDatabase`, and structural anchor blocks at their exact
/// positions as well as real Notion IDs of matched blocks) and updates `detail.markdown`,
/// `detail.blocks`, `detail.page.plain_text`, and `detail.media` in < 1 ms without waiting
/// for any Notion API HTTP roundtrip.
pub fn apply_optimistic_markdown_to_detail(detail: &mut NotionPageDetail, edited_markdown: &str) {
    let normalized = edited_markdown.replace("\r\n", "\n").replace('\r', "\n");
    let page_id = detail.page.id.clone();

    let payloads = markdown_to_notion_blocks(&normalized);
    let new_parsed: Vec<NotionBlock> = payloads
        .iter()
        .map(|val| parse_notion_block(val, &page_id, None))
        .collect();

    let (old_segments, anchors, new_ranges) =
        partition_blocks_around_anchors(&detail.blocks, &new_parsed);

    let mut final_blocks: Vec<NotionBlock> = Vec::with_capacity(new_parsed.len() + anchors.len());
    let mut opt_counter = 0usize;

    for (seg_idx, (old_editable, range)) in
        old_segments.into_iter().zip(new_ranges.into_iter()).enumerate()
    {
        let mut seg_new: Vec<NotionBlock> = new_parsed[range].to_vec();
        let old_keys: Vec<(String, String)> = old_editable.iter().map(block_comparison_key).collect();
        let new_keys: Vec<(String, String)> = seg_new.iter().map(block_comparison_key).collect();

        let mut prefix_len = 0usize;
        while prefix_len < old_keys.len()
            && prefix_len < new_keys.len()
            && old_keys[prefix_len] == new_keys[prefix_len]
        {
            seg_new[prefix_len].id = old_editable[prefix_len].id.clone();
            seg_new[prefix_len].has_children = old_editable[prefix_len].has_children;
            seg_new[prefix_len].children = old_editable[prefix_len].children.clone();
            prefix_len += 1;
        }

        let mut suffix_len = 0usize;
        while suffix_len < (old_keys.len() - prefix_len)
            && suffix_len < (new_keys.len() - prefix_len)
            && old_keys[old_keys.len() - 1 - suffix_len]
                == new_keys[new_keys.len() - 1 - suffix_len]
        {
            let old_idx = old_editable.len() - 1 - suffix_len;
            let new_idx = seg_new.len() - 1 - suffix_len;
            seg_new[new_idx].id = old_editable[old_idx].id.clone();
            seg_new[new_idx].has_children = old_editable[old_idx].has_children;
            seg_new[new_idx].children = old_editable[old_idx].children.clone();
            suffix_len += 1;
        }

        let old_mid_end = old_editable.len() - suffix_len;
        let new_mid_end = seg_new.len() - suffix_len;
        if old_mid_end - prefix_len == new_mid_end - prefix_len {
            for offset in 0..(old_mid_end - prefix_len) {
                let old_blk = &old_editable[prefix_len + offset];
                let new_blk = &mut seg_new[prefix_len + offset];
                if old_blk.block_type == new_blk.block_type && !old_blk.id.is_empty() {
                    new_blk.id = old_blk.id.clone();
                }
            }
        }

        for blk in &mut seg_new {
            if blk.id.trim().is_empty() {
                blk.id = format!("__optimistic_{}_{}", page_id, opt_counter);
                opt_counter += 1;
            }
        }

        final_blocks.extend(seg_new);
        if let Some(anchor) = anchors.get(seg_idx) {
            final_blocks.push(anchor.clone());
        }
    }

    detail.page.plain_text = blocks_to_plain_text(&final_blocks);
    detail.media = collect_media_refs(&final_blocks);
    detail.blocks = final_blocks;
    detail.markdown = normalized;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_cells_with_pipes_and_newlines_round_trip() {
        let row = |cells: &[&str]| NotionBlock {
            id: format!("row-{}", cells[0]),
            block_type: "table_row".into(),
            kind: NotionBlockKind::TableRow {
                cells: cells.iter().map(|c| vec![RichTextSpan::plain(*c)]).collect(),
            },
            ..Default::default()
        };
        let table = NotionBlock {
            id: "tbl".into(),
            block_type: "table".into(),
            has_children: true,
            kind: NotionBlockKind::Table {
                table_width: 2,
                has_column_header: true,
                has_row_header: false,
            },
            children: vec![row(&["Key", "Value"]), row(&["a|b", "line 1\nline 2"])],
            ..Default::default()
        };
        let md = blocks_to_markdown(&[table]);
        assert!(md.contains("| a\\|b | line 1<br>line 2 |"), "{md}");
        let parsed = markdown_to_notion_blocks(&md);
        assert_eq!(parsed.len(), 1, "{parsed:?}");
        let rows = parsed[0]["table"]["children"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        let cells = rows[1]["table_row"]["cells"].as_array().unwrap();
        assert_eq!(cells.len(), 2, "the pipe stays inside one cell");
        assert_eq!(cells[0][0]["text"]["content"], "a|b");
    }

    #[test]
    fn test_markdown_to_notion_blocks_and_back() {
        let md = "# Roadmap\n\n- [ ] Ship v0.1.0\n- [x] Build SQLite cache\n- Bullet item\n1. First step\n> 💡 Tip: Use Ctrl+K\n> Classic quote\n---\n```rs\nfn main() {}\n```\nHello **Notion** with `code` and [Docs](https://developers.notion.com).";
        let payloads = markdown_to_notion_blocks(md);
        assert_eq!(payloads.len(), 10);
        assert_eq!(payloads[0]["type"], "heading_1");
        assert_eq!(payloads[1]["type"], "to_do");
        assert_eq!(payloads[1]["to_do"]["checked"], false);
        assert_eq!(payloads[2]["to_do"]["checked"], true);
        assert_eq!(payloads[3]["type"], "bulleted_list_item");
        assert_eq!(payloads[4]["type"], "numbered_list_item");
        assert_eq!(payloads[5]["type"], "callout");
        assert_eq!(payloads[6]["type"], "quote");
        assert_eq!(payloads[7]["type"], "divider");
        assert_eq!(payloads[8]["type"], "code");
        assert_eq!(payloads[8]["code"]["language"], "rust");
        assert_eq!(payloads[9]["type"], "paragraph");

        let parsed: Vec<NotionBlock> = payloads
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let mut v = p.clone();
                v["id"] = json!(format!("blk-{i}"));
                parse_notion_block(&v, "page-1", None)
            })
            .collect();
        let exported = blocks_to_markdown(&parsed);
        assert!(exported.contains("# Roadmap"));
        assert!(exported.contains("- [ ] Ship v0.1.0"));
        assert!(exported.contains("- [x] Build SQLite cache"));
        assert!(exported.contains("```rust"));
        assert!(exported.contains("**Notion**"));
        assert!(exported.contains("[Docs](https://developers.notion.com)"));
    }

    #[test]
    fn test_slash_commands() {
        let h1 = parse_slash_command_blocks("/h1 Sprint Goals");
        assert_eq!(h1[0]["type"], "heading_1");

        let todo = parse_slash_command_blocks("/todo Review PR #42");
        assert_eq!(todo[0]["type"], "to_do");

        let callout = parse_slash_command_blocks("/callout 🔥 Hotfix ready");
        assert_eq!(callout[0]["type"], "callout");
        assert_eq!(callout[0]["callout"]["icon"]["emoji"], "🔥");

        let toggle = parse_slash_command_blocks("/toggle Architecture Notes");
        assert_eq!(toggle[0]["type"], "toggle");

        let code = parse_slash_command_blocks("/code rust fn hi() {}");
        assert_eq!(code[0]["type"], "code");
        assert_eq!(code[0]["code"]["language"], "rust");
    }

    #[test]
    fn test_parse_notion_page_and_database_properties() {
        let page_json = json!({
            "id": "page-1",
            "url": "https://notion.so/page-1",
            "icon": { "type": "emoji", "emoji": "🚀" },
            "parent": { "type": "database_id", "database_id": "db-1" },
            "properties": {
                "Name": { "type": "title", "title": [{ "plain_text": "Launch Plan" }] },
                "Status": { "type": "status", "status": { "name": "In Progress", "color": "blue" } },
                "Tags": { "type": "multi_select", "multi_select": [{ "name": "CLI", "color": "purple" }, { "name": "Rust", "color": "orange" }] },
                "Done": { "type": "checkbox", "checkbox": false }
            }
        });
        let page = parse_notion_page(&page_json, "ws-1").unwrap();
        assert_eq!(page.title, "Launch Plan");
        assert_eq!(page.icon.as_deref(), Some("🚀"));
        assert_eq!(page.parent_type, "database_id");
        assert!(page.properties_summary().contains("In Progress"));
        assert!(page.properties_summary().contains("CLI, Rust"));
    }

    #[test]
    fn rollup_array_and_date_range_render_values() {
        let arr = parse_property_value(&json!({
            "type": "rollup",
            "rollup": {
                "type": "array",
                "array": [
                    { "type": "number", "number": 10 },
                    { "type": "number", "number": 20 }
                ]
            }
        }));
        assert_eq!(arr, NotionPropertyValue::Rollup("10, 20".into()));

        let empty = parse_property_value(&json!({
            "type": "rollup",
            "rollup": { "type": "array", "array": [] }
        }));
        assert_eq!(empty, NotionPropertyValue::Rollup(String::new()));

        let num = parse_property_value(&json!({
            "type": "rollup",
            "rollup": { "type": "number", "number": 30 }
        }));
        assert_eq!(num, NotionPropertyValue::Rollup("30".into()));

        let range = parse_property_value(&json!({
            "type": "rollup",
            "rollup": {
                "type": "date",
                "date": { "start": "2026-01-01", "end": "2026-01-05" }
            }
        }));
        assert_eq!(
            range,
            NotionPropertyValue::Rollup("2026-01-01 -> 2026-01-05".into())
        );

        let single_date = parse_property_value(&json!({
            "type": "rollup",
            "rollup": {
                "type": "date",
                "date": { "start": "2026-01-01", "end": null }
            }
        }));
        assert_eq!(single_date, NotionPropertyValue::Rollup("2026-01-01".into()));

        let incomplete = parse_property_value(&json!({
            "type": "rollup",
            "rollup": { "type": "incomplete" }
        }));
        assert_eq!(incomplete, NotionPropertyValue::Rollup(String::new()));
    }

    #[test]
    fn other_user_and_verification_keep_values() {
        assert_eq!(
            parse_property_value(&json!({
                "type": "created_by",
                "created_by": { "name": "Ada" }
            })),
            NotionPropertyValue::Other("Ada".into())
        );
        assert_eq!(
            parse_property_value(&json!({
                "type": "last_edited_by",
                "last_edited_by": { "id": "u1" }
            })),
            NotionPropertyValue::Other("u1".into())
        );
        assert_eq!(
            parse_property_value(&json!({
                "type": "verification",
                "verification": { "state": "verified" }
            })),
            NotionPropertyValue::Other("verified".into())
        );
        assert_eq!(
            parse_property_value(&json!({
                "type": "button",
                "button": {}
            })),
            NotionPropertyValue::Other(String::new())
        );
    }

    #[test]
    fn incomplete_block_ids_reports_unloaded_children_in_order() {
        let blocks = vec![
            NotionBlock {
                id: "blk-child-page".into(),
                block_type: "child_page".into(),
                has_children: true,
                ..Default::default()
            },
            NotionBlock {
                id: "blk-loaded-parent".into(),
                block_type: "toggle".into(),
                has_children: true,
                children: vec![NotionBlock {
                    id: "blk-nested-missing".into(),
                    block_type: "toggle".into(),
                    has_children: true,
                    ..Default::default()
                }],
                ..Default::default()
            },
            NotionBlock {
                id: "blk-top-missing".into(),
                block_type: "bulleted_list_item".into(),
                has_children: true,
                ..Default::default()
            },
        ];
        assert_eq!(
            incomplete_block_ids(&blocks),
            vec!["blk-nested-missing".to_string(), "blk-top-missing".to_string()]
        );
    }

    #[test]
    fn database_schema_preserves_relation_database_id_and_omits_it_for_other_columns() {
        let db_json = json!({
            "object": "database",
            "id": "dddddddd-dddd-dddd-dddd-ddddddddddd1",
            "url": "https://www.notion.so/db-sprint",
            "parent": { "type": "workspace", "workspace": true },
            "title": [{ "plain_text": "Sprint Board" }],
            "properties": {
                "Task Title": { "id": "title", "type": "title", "title": {} },
                "Project": {
                    "id": "rel",
                    "type": "relation",
                    "relation": {
                        "database_id": "dddddddd-dddd-dddd-dddd-ddddddddddd2",
                        "type": "single_property"
                    }
                },
                "Priority": {
                    "id": "sel",
                    "type": "select",
                    "select": { "options": [{ "id": "1", "name": "High", "color": "red" }] }
                }
            }
        });

        let db = parse_notion_database(&db_json, "ws-1").unwrap();
        assert_eq!(
            db.schema["Project"].relation_database_id.as_deref(),
            Some("dddddddd-dddd-dddd-dddd-ddddddddddd2")
        );
        assert_eq!(db.schema["Priority"].relation_database_id, None);

        let serialized = serde_json::to_value(&db.schema).unwrap();
        assert_eq!(
            serialized["Project"]["relation_database_id"],
            "dddddddd-dddd-dddd-dddd-ddddddddddd2"
        );
        assert!(serialized["Priority"].get("relation_database_id").is_none());
    }

    #[test]
    fn markdown_table_converts_to_notion_table_and_table_rows() {
        let md = "\
| Area | Status | Owner |
| --- | :---: | ---: |
| **CLI** | Ready | Alice |
| MCP | Audit | Bob | Extra |
| Docs | Draft |

| Not a table without separator |";
        let blocks = markdown_to_notion_blocks(md);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "table");
        assert_eq!(blocks[0]["table"]["table_width"], 3);
        assert_eq!(blocks[0]["table"]["has_column_header"], true);
        let rows = blocks[0]["table"]["children"].as_array().unwrap();
        assert_eq!(rows.len(), 4); // header + 3 body rows
        assert_eq!(rows[0]["type"], "table_row");
        assert_eq!(rows[0]["table_row"]["cells"][0][0]["text"]["content"], "Area");
        assert_eq!(rows[1]["table_row"]["cells"][0][0]["text"]["content"], "CLI");
        assert_eq!(rows[1]["table_row"]["cells"][0][0]["annotations"]["bold"], true);
        // Truncated extra cell to width 3
        assert_eq!(rows[2]["table_row"]["cells"].as_array().unwrap().len(), 3);
        // Padded missing cell to width 3
        assert_eq!(rows[3]["table_row"]["cells"].as_array().unwrap().len(), 3);
        assert_eq!(rows[3]["table_row"]["cells"][2][0]["text"]["content"], "");

        // Pipe line without separator remains paragraph
        assert_eq!(blocks[1]["type"], "paragraph");
    }

    #[test]
    fn markdown_nested_lists_attach_children_with_two_level_cap_and_flat_control() {
        // Nested list (2-space and tab indentation, including depth 3 clamped to level 2 sibling)
        let nested_md = "\
- Top item 1
  - Nested bullet item level 2
    - Level 3 clamped sibling
\t- [x] Nested checked todo
- Top item 2";
        let nested = markdown_to_notion_blocks(nested_md);
        assert_eq!(nested.len(), 2);
        assert_eq!(nested[0]["type"], "bulleted_list_item");
        let children = nested[0]["bulleted_list_item"]["children"]
            .as_array()
            .unwrap();
        assert_eq!(children.len(), 3);
        assert_eq!(children[0]["type"], "bulleted_list_item");
        assert_eq!(
            children[0]["bulleted_list_item"]["rich_text"][0]["text"]["content"],
            "Nested bullet item level 2"
        );
        assert!(children[0]["bulleted_list_item"].get("children").is_none());
        assert_eq!(
            children[1]["bulleted_list_item"]["rich_text"][0]["text"]["content"],
            "Level 3 clamped sibling"
        );
        assert_eq!(children[2]["type"], "to_do");
        assert_eq!(children[2]["to_do"]["checked"], true);
        assert!(nested[1]["bulleted_list_item"].get("children").is_none());

        // Control: flat list produces identical top-level blocks without children keys
        let flat_md = "- Alpha\n- Beta\n1. One\n- [ ] Task";
        let flat = markdown_to_notion_blocks(flat_md);
        assert_eq!(flat.len(), 4);
        assert_eq!(flat[0]["type"], "bulleted_list_item");
        assert!(flat[0]["bulleted_list_item"].get("children").is_none());
        assert_eq!(flat[1]["type"], "bulleted_list_item");
        assert!(flat[1]["bulleted_list_item"].get("children").is_none());
        assert_eq!(flat[2]["type"], "numbered_list_item");
        assert!(flat[2]["numbered_list_item"].get("children").is_none());
        assert_eq!(flat[3]["type"], "to_do");
        assert!(flat[3]["to_do"].get("children").is_none());
    }

    #[test]
    fn parser_preserves_file_urls_unknown_props_rollup_variants_and_page_authors() {
        // 1. files property preserves hosted and external URLs alongside names
        let files_prop = parse_property_value(&json!({
            "type": "files",
            "files": [
                {
                    "name": "spec.pdf",
                    "type": "file",
                    "file": { "url": "https://files.notion.so/spec.pdf" }
                },
                {
                    "name": "diagram.png",
                    "type": "external",
                    "external": { "url": "https://cdn.example.com/diagram.png" }
                },
                {
                    "name": "legacy-only.txt"
                },
                {
                    "type": "external",
                    "external": { "url": "https://cdn.example.com/nameless.png" }
                }
            ]
        }));
        assert_eq!(
            files_prop,
            NotionPropertyValue::Files(vec![
                "[spec.pdf](https://files.notion.so/spec.pdf)".into(),
                "[diagram.png](https://cdn.example.com/diagram.png)".into(),
                "legacy-only.txt".into(),
                "https://cdn.example.com/nameless.png".into(),
            ])
        );

        // 2. unknown property types preserve compact JSON/string summary instead of Other("")
        let place_prop = parse_property_value(&json!({
            "type": "place",
            "place": { "name": "Seoul", "lat": 37.5 }
        }));
        match place_prop {
            NotionPropertyValue::Other(ref s) => {
                let parsed: Value = serde_json::from_str(s).unwrap();
                assert_eq!(parsed["name"], "Seoul");
                assert_eq!(parsed["lat"], 37.5);
            }
            other => panic!("expected Other, got {other:?}"),
        }
        assert_eq!(
            parse_property_value(&json!({
                "type": "custom_string",
                "custom_string": "hello world"
            })),
            NotionPropertyValue::Other("hello world".into())
        );

        // 3. rollup preserves non-number/date/array values (string, boolean, structured)
        assert_eq!(
            parse_property_value(&json!({
                "type": "rollup",
                "rollup": { "type": "string", "string": "Approved" }
            })),
            NotionPropertyValue::Rollup("Approved".into())
        );
        assert_eq!(
            parse_property_value(&json!({
                "type": "rollup",
                "rollup": { "type": "boolean", "boolean": true }
            })),
            NotionPropertyValue::Rollup("true".into())
        );
        assert_eq!(
            parse_property_value(&json!({
                "type": "rollup",
                "rollup": { "type": "unsupported", "unsupported": { "reason": "formula" } }
            })),
            NotionPropertyValue::Rollup(r#"{"reason":"formula"}"#.into())
        );

        // 4. parse_notion_page captures created_by and last_edited_by (name or id)
        let page_json = json!({
            "id": "page-authors",
            "url": "https://notion.so/page-authors",
            "parent": { "type": "workspace", "workspace": true },
            "created_by": { "object": "user", "id": "u-1", "name": "Alice Kim" },
            "last_edited_by": { "object": "user", "id": "u-2" },
            "properties": {
                "title": { "type": "title", "title": [{ "plain_text": "Authored" }] }
            }
        });
        let page = parse_notion_page(&page_json, "ws-1").unwrap();
        assert_eq!(page.created_by.as_deref(), Some("Alice Kim"));
        assert_eq!(page.last_edited_by.as_deref(), Some("u-2"));

        // Absent authors serialize cleanly (skipped when None)
        let minimal = parse_notion_page(
            &json!({
                "id": "page-min",
                "properties": { "title": { "type": "title", "title": [{ "plain_text": "Min" }] } }
            }),
            "ws-1",
        )
        .unwrap();
        assert_eq!(minimal.created_by, None);
        assert_eq!(minimal.last_edited_by, None);
        let ser = serde_json::to_value(&minimal).unwrap();
        assert!(ser.get("created_by").is_none());
        assert!(ser.get("last_edited_by").is_none());
    }
}
