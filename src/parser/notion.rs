//! Notion Block & Rich-Text Engine:
//! - Parses Notion REST API v1 JSON into strongly typed Pages, Databases, Properties, Blocks, Rich-Text Spans, and Comments.
//! - Exports Notion block trees into clean GitHub-Flavored Markdown (`blocks_to_markdown`).
//! - Converts Markdown or slash-style quick-append input (`/h1`, `/todo`, `/callout`, `/code`, `/quote`, `/toggle`)
//!   into valid Notion API block payloads (`markdown_to_notion_blocks`, `parse_slash_command_blocks`).

use std::collections::BTreeMap;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::{
    img_preview::is_image_mime_or_name,
    models::{
        spans_plain_text, NotionBlock, NotionBlockKind, NotionColor, NotionComment, NotionDatabase,
        NotionMediaRef, NotionPage, NotionPropertySchema, NotionPropertyValue, RichTextAnnotations,
        RichTextKind, RichTextSpan, SelectOption,
    },
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

fn parse_parent(val: Option<&Value>) -> (String, Option<String>) {
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
                        .filter_map(|f| f.get("name").and_then(Value::as_str).map(str::to_string))
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
            let r = val.get("rollup");
            let s = r
                .and_then(|x| {
                    x.get("number")
                        .and_then(Value::as_f64)
                        .map(|n| n.to_string())
                        .or_else(|| {
                            x.get("date")
                                .and_then(|d| d.get("start"))
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        })
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
        _ => NotionPropertyValue::Other(String::new()),
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
            .or_else(|| val.get("in_trash"))
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
            .or_else(|| val.get("in_trash"))
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
        other => NotionBlockKind::Unsupported {
            raw_type: other.to_string(),
        },
    };

    let children = val
        .get("children")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|c| parse_notion_block(c, page_id, Some(&id)))
                .collect()
        })
        .unwrap_or_default();

    NotionBlock {
        id,
        page_id: page_id.to_string(),
        parent_block_id: parent_block_id.map(str::to_string),
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
                let mime_type = guess_mime_from_url(url, true);
                out.push(NotionMediaRef {
                    block_id: blk.id.clone(),
                    url: url.clone(),
                    display_name,
                    mime_type,
                    is_image: true,
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
                let mime_type = guess_mime_from_url(url, false);
                let is_img = is_image_mime_or_name(&mime_type, Some(&display_name));
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

fn guess_mime_from_url(url: &str, default_image: bool) -> String {
    let lower = url
        .split('?')
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    if lower.ends_with(".png") {
        "image/png".to_string()
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg".to_string()
    } else if lower.ends_with(".gif") {
        "image/gif".to_string()
    } else if lower.ends_with(".webp") {
        "image/webp".to_string()
    } else if lower.ends_with(".pdf") {
        "application/pdf".to_string()
    } else if default_image {
        "image/png".to_string()
    } else {
        "application/octet-stream".to_string()
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

/// Exports a Notion block tree to clean, editable Markdown.
pub fn blocks_to_markdown(blocks: &[NotionBlock]) -> String {
    let mut lines = Vec::new();
    render_blocks_md(blocks, 0, &mut lines);
    lines.join("\n")
}

fn render_blocks_md(blocks: &[NotionBlock], depth: usize, out: &mut Vec<String>) {
    let indent = "  ".repeat(depth);
    let mut num_counter = 0usize;

    for blk in blocks {
        if !matches!(blk.kind, NotionBlockKind::NumberedListItem { .. }) {
            num_counter = 0;
        }
        match &blk.kind {
            NotionBlockKind::Heading1 { rich_text, .. } => {
                out.push(format!("{indent}# {}", rich_text_to_markdown(rich_text)));
                out.push(String::new());
            }
            NotionBlockKind::Heading2 { rich_text, .. } => {
                out.push(format!("{indent}## {}", rich_text_to_markdown(rich_text)));
                out.push(String::new());
            }
            NotionBlockKind::Heading3 { rich_text, .. } => {
                out.push(format!("{indent}### {}", rich_text_to_markdown(rich_text)));
                out.push(String::new());
            }
            NotionBlockKind::Paragraph { rich_text, .. } => {
                let text = rich_text_to_markdown(rich_text);
                if text.is_empty() {
                    out.push(String::new());
                } else {
                    out.push(format!("{indent}{text}"));
                    out.push(String::new());
                }
            }
            NotionBlockKind::BulletedListItem { rich_text, .. } => {
                out.push(format!("{indent}- {}", rich_text_to_markdown(rich_text)));
                if !blk.children.is_empty() {
                    render_blocks_md(&blk.children, depth + 1, out);
                }
            }
            NotionBlockKind::NumberedListItem { rich_text, .. } => {
                num_counter += 1;
                out.push(format!(
                    "{indent}{num_counter}. {}",
                    rich_text_to_markdown(rich_text)
                ));
                if !blk.children.is_empty() {
                    render_blocks_md(&blk.children, depth + 1, out);
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
                    render_blocks_md(&blk.children, depth + 1, out);
                }
            }
            NotionBlockKind::Toggle { rich_text, .. } => {
                out.push(format!("{indent}> ▸ {}", rich_text_to_markdown(rich_text)));
                if !blk.children.is_empty() {
                    render_blocks_md(&blk.children, depth + 1, out);
                }
                out.push(String::new());
            }
            NotionBlockKind::Callout {
                rich_text, icon, ..
            } => {
                let ic = icon.as_deref().unwrap_or("💡");
                out.push(format!("{indent}> {ic} {}", rich_text_to_markdown(rich_text)));
                if !blk.children.is_empty() {
                    render_blocks_md(&blk.children, depth + 1, out);
                }
                out.push(String::new());
            }
            NotionBlockKind::Quote { rich_text, .. } => {
                out.push(format!("{indent}> {}", rich_text_to_markdown(rich_text)));
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
                out.push(spans_plain_text(rich_text));
                out.push(format!("{indent}```"));
                out.push(String::new());
            }
            NotionBlockKind::Table { .. } => {
                for (row_idx, row) in blk.children.iter().enumerate() {
                    if let NotionBlockKind::TableRow { cells } = &row.kind {
                        let cols: Vec<String> =
                            cells.iter().map(|c| rich_text_to_markdown(c)).collect();
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
                let cols: Vec<String> = cells.iter().map(|c| rich_text_to_markdown(c)).collect();
                out.push(format!("{indent}| {} |", cols.join(" | ")));
            }
            NotionBlockKind::Image { url, caption, .. } => {
                let alt = spans_plain_text(caption);
                out.push(format!("{indent}![{alt}]({url})"));
                out.push(String::new());
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
            NotionBlockKind::ColumnList
            | NotionBlockKind::Column
            | NotionBlockKind::SyncedBlock { .. } => {
                render_blocks_md(&blk.children, depth, out);
            }
            NotionBlockKind::TableOfContents { .. } => {
                out.push(format!("{indent}[Table of Contents]"));
                out.push(String::new());
            }
            NotionBlockKind::Unsupported { raw_type } => {
                out.push(format!("{indent}[unsupported: {raw_type}]"));
            }
        }
    }
}

/// Converts inline Markdown (`**bold**`, `*italic*`, `` `code` ``, `~~strike~~`, `[text](url)`)
/// into a Notion API `rich_text` JSON array.
pub fn parse_inline_markdown_rich_text(input: &str) -> Vec<Value> {
    let mut spans = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let len = chars.len();
    let mut i = 0usize;
    let mut plain_buf = String::new();

    let flush_plain = |buf: &mut String, out: &mut Vec<Value>| {
        if !buf.is_empty() {
            out.push(json!({
                "type": "text",
                "text": { "content": std::mem::take(buf) }
            }));
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
                            flush_plain(&mut plain_buf, &mut spans);
                            spans.push(json!({
                                "type": "text",
                                "text": { "content": label, "link": { "url": href } }
                            }));
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
                flush_plain(&mut plain_buf, &mut spans);
                spans.push(json!({
                    "type": "text",
                    "text": { "content": code },
                    "annotations": { "code": true }
                }));
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
                flush_plain(&mut plain_buf, &mut spans);
                spans.push(json!({
                    "type": "text",
                    "text": { "content": text },
                    "annotations": { "bold": true }
                }));
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
                flush_plain(&mut plain_buf, &mut spans);
                spans.push(json!({
                    "type": "text",
                    "text": { "content": text },
                    "annotations": { "strikethrough": true }
                }));
                i = end + 2;
                continue;
            }
        }

        // Italic: *text*
        if chars[i] == '*' {
            if let Some(end) = (i + 1..len).find(|&j| chars[j] == '*') {
                if end > i + 1 {
                    let text: String = chars[i + 1..end].iter().collect();
                    flush_plain(&mut plain_buf, &mut spans);
                    spans.push(json!({
                        "type": "text",
                        "text": { "content": text },
                        "annotations": { "italic": true }
                    }));
                    i = end + 1;
                    continue;
                }
            }
        }

        plain_buf.push(chars[i]);
        i += 1;
    }

    flush_plain(&mut plain_buf, &mut spans);
    if spans.is_empty() {
        spans.push(json!({ "type": "text", "text": { "content": "" } }));
    }
    spans
}

/// Parses Markdown text into Notion REST API v1 block payloads (`heading_1/2/3`, `to_do`,
/// `bulleted_list_item`, `numbered_list_item`, `code`, `quote`, `callout`, `toggle`, `divider`, `paragraph`).
pub fn markdown_to_notion_blocks(markdown: &str) -> Vec<Value> {
    let mut blocks = Vec::new();
    let lines: Vec<&str> = markdown.lines().collect();
    let mut i = 0usize;

    while i < lines.len() {
        let raw = lines[i];
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            i += 1;
            continue;
        }

        // Fenced code block ```lang
        if let Some(lang_raw) = trimmed.strip_prefix("```") {
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

        // Divider --- or ***
        if trimmed == "---" || trimmed == "***" || trimmed == "___" {
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
            blocks.push(json!({
                "object": "block",
                "type": "heading_3",
                "heading_3": { "rich_text": parse_inline_markdown_rich_text(rest.trim()) }
            }));
            i += 1;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("## ") {
            blocks.push(json!({
                "object": "block",
                "type": "heading_2",
                "heading_2": { "rich_text": parse_inline_markdown_rich_text(rest.trim()) }
            }));
            i += 1;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("# ") {
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
            blocks.push(json!({
                "object": "block",
                "type": "to_do",
                "to_do": {
                    "rich_text": parse_inline_markdown_rich_text(rest.trim()),
                    "checked": false
                }
            }));
            i += 1;
            continue;
        }
        if let Some(rest) = trimmed
            .strip_prefix("- [x] ")
            .or_else(|| trimmed.strip_prefix("- [X] "))
            .or_else(|| trimmed.strip_prefix("* [x] "))
            .or_else(|| trimmed.strip_prefix("* [X] "))
        {
            blocks.push(json!({
                "object": "block",
                "type": "to_do",
                "to_do": {
                    "rich_text": parse_inline_markdown_rich_text(rest.trim()),
                    "checked": true
                }
            }));
            i += 1;
            continue;
        }

        // Bulleted list
        if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            blocks.push(json!({
                "object": "block",
                "type": "bulleted_list_item",
                "bulleted_list_item": {
                    "rich_text": parse_inline_markdown_rich_text(rest.trim())
                }
            }));
            i += 1;
            continue;
        }

        // Numbered list: 1. text
        if let Some(rest) = strip_numbered_prefix(trimmed) {
            blocks.push(json!({
                "object": "block",
                "type": "numbered_list_item",
                "numbered_list_item": {
                    "rich_text": parse_inline_markdown_rich_text(rest.trim())
                }
            }));
            i += 1;
            continue;
        }

        // Quote / Callout / Toggle: `> 💡 text`, `> ▸ toggle`, `> quote`
        if let Some(rest) = trimmed.strip_prefix("> ") {
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

        // Image: ![alt](https://...)
        if trimmed.starts_with("![") && trimmed.ends_with(')') {
            if let Some(mid) = trimmed.find("](") {
                let url = &trimmed[mid + 2..trimmed.len() - 1];
                if url.starts_with("http://") || url.starts_with("https://") {
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

fn strip_numbered_prefix(s: &str) -> Option<&str> {
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

fn split_leading_emoji(s: &str) -> Option<(String, &str)> {
    let mut chars = s.chars();
    let first = chars.next()?;
    let cp = first as u32;
    let is_emoji = (0x1F300..=0x1FAFF).contains(&cp)
        || (0x2600..=0x27BF).contains(&cp)
        || first == '⚠'
        || first == 'ℹ';
    if !is_emoji {
        return None;
    }
    let mut end = first.len_utf8();
    // Include variation selector FE0F if present
    if s[end..].starts_with('\u{FE0F}') {
        end += '\u{FE0F}'.len_utf8();
    }
    let rest = s[end..].trim_start();
    Some((s[..end].to_string(), rest))
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
