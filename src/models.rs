//! Domain models for standalone `@bitefoundry/notion` — Pages, Blocks, Rich Text, Databases,
//! Comments, Workspaces, Search Hits, and Sync Events.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Canonical summary emitted when background polling is retrying after transient errors.
pub const POLL_RETRY_SUMMARY: &str = "retrying";

/// Notion's 10 canonical semantic colors (used for pills, callouts, and rich-text annotations).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotionColor {
    #[default]
    Default,
    Gray,
    Brown,
    Orange,
    Yellow,
    Green,
    Blue,
    Purple,
    Pink,
    Red,
}

impl NotionColor {
    pub fn from_notion_str(raw: Option<&str>) -> Self {
        let Some(s) = raw else {
            return Self::Default;
        };
        let base = s.trim().to_ascii_lowercase();
        let base = base.strip_suffix("_background").unwrap_or(&base);
        match base {
            "gray" | "grey" => Self::Gray,
            "brown" => Self::Brown,
            "orange" => Self::Orange,
            "yellow" => Self::Yellow,
            "green" => Self::Green,
            "blue" => Self::Blue,
            "purple" => Self::Purple,
            "pink" => Self::Pink,
            "red" => Self::Red,
            _ => Self::Default,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Gray => "gray",
            Self::Brown => "brown",
            Self::Orange => "orange",
            Self::Yellow => "yellow",
            Self::Green => "green",
            Self::Blue => "blue",
            Self::Purple => "purple",
            Self::Pink => "pink",
            Self::Red => "red",
        }
    }
}

/// Rich-text formatting annotations from the Notion API.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RichTextAnnotations {
    pub bold: bool,
    pub italic: bool,
    pub strikethrough: bool,
    pub underline: bool,
    pub code: bool,
    pub color: NotionColor,
    pub is_background: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RichTextKind {
    #[default]
    Text,
    Mention {
        mention_type: String,
        target_id: Option<String>,
    },
    Equation {
        expression: String,
    },
}

/// A single styled Notion rich-text span.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RichTextSpan {
    pub plain_text: String,
    pub href: Option<String>,
    pub annotations: RichTextAnnotations,
    pub kind: RichTextKind,
}

impl RichTextSpan {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            plain_text: text.into(),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NotionUser {
    pub id: String,
    pub name: String,
    pub user_type: String,
    pub avatar_url: Option<String>,
    pub email: Option<String>,
    pub workspace_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NotionParent {
    /// `"workspace"`, `"page_id"`, `"database_id"`, or `"block_id"`.
    pub parent_type: String,
    pub id: Option<String>,
}

/// Select / multi-select / status pill option with canonical Notion color.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SelectOption {
    pub id: Option<String>,
    pub name: String,
    pub color: NotionColor,
}

/// Parsed Notion page property value (for database rows and page properties strips).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum NotionPropertyValue {
    Title(String),
    RichText(String),
    Status(Option<SelectOption>),
    Select(Option<SelectOption>),
    MultiSelect(Vec<SelectOption>),
    Date {
        start: Option<String>,
        end: Option<String>,
    },
    Checkbox(bool),
    Number(Option<f64>),
    People(Vec<String>),
    Url(Option<String>),
    Email(Option<String>),
    PhoneNumber(Option<String>),
    Files(Vec<String>),
    Relation(Vec<String>),
    Formula(String),
    Rollup(String),
    CreatedTime(String),
    LastEditedTime(String),
    UniqueId(String),
    Other(String),
}

impl NotionPropertyValue {
    pub fn to_plain_string(&self) -> String {
        match self {
            Self::Title(s)
            | Self::RichText(s)
            | Self::Formula(s)
            | Self::Rollup(s)
            | Self::CreatedTime(s)
            | Self::LastEditedTime(s)
            | Self::UniqueId(s)
            | Self::Other(s) => s.clone(),
            Self::Status(opt) | Self::Select(opt) => {
                opt.as_ref().map(|o| o.name.clone()).unwrap_or_default()
            }
            Self::MultiSelect(opts) => opts
                .iter()
                .map(|o| o.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            Self::Date { start, end } => match (start.as_deref(), end.as_deref()) {
                (Some(s), Some(e)) => format!("{s} -> {e}"),
                (Some(s), None) => s.to_string(),
                _ => String::new(),
            },
            Self::Checkbox(b) => if *b { "true" } else { "false" }.to_string(),
            Self::Number(Some(n)) => {
                if n.fract() == 0.0 {
                    format!("{}", *n as i64)
                } else {
                    format!("{n:.2}")
                }
            }
            Self::Number(None) => String::new(),
            Self::People(list) | Self::Files(list) | Self::Relation(list) => list.join(", "),
            Self::Url(v) | Self::Email(v) | Self::PhoneNumber(v) => v.clone().unwrap_or_default(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::Checkbox(_) => false,
            _ => self.to_plain_string().trim().is_empty(),
        }
    }
}

/// Database property schema column definition.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NotionPropertySchema {
    pub id: String,
    pub name: String,
    pub prop_type: String,
    pub options: Vec<SelectOption>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relation_database_id: Option<String>,
}

/// Notion Page metadata + properties.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NotionPage {
    pub id: String,
    pub workspace_id: String,
    pub parent_type: String,
    pub parent_id: Option<String>,
    pub title: String,
    pub icon: Option<String>,
    pub cover_url: Option<String>,
    pub url: String,
    pub archived: bool,
    pub is_favorite: bool,
    pub properties: BTreeMap<String, NotionPropertyValue>,
    pub plain_text: String,
    pub created_time: Option<String>,
    pub last_edited_time: Option<String>,
    pub synced_at: Option<String>,
}

impl NotionPage {
    pub fn display_icon(&self) -> &str {
        page_display_icon(self.icon.as_deref())
    }

    pub fn display_title(&self) -> &str {
        page_display_title(&self.title)
    }

    /// Concatenates property names and values for FTS5 search indexing.
    pub fn properties_summary(&self) -> String {
        let mut parts = Vec::new();
        for (k, v) in &self.properties {
            let s = v.to_plain_string();
            if !s.trim().is_empty() {
                parts.push(format!("{k}: {s}"));
            }
        }
        parts.join(" · ")
    }
}

/// Page metadata used by the workspace tree and its recent-page search suggestions.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NotionPageSummary {
    pub id: String,
    pub workspace_id: String,
    pub parent_type: String,
    pub parent_id: Option<String>,
    pub title: String,
    pub icon: Option<String>,
    pub url: String,
    pub archived: bool,
    pub is_favorite: bool,
    pub properties_summary: String,
    pub last_edited_time: Option<String>,
}

impl NotionPageSummary {
    pub fn display_title(&self) -> &str {
        page_display_title(&self.title)
    }

    pub fn display_icon(&self) -> &str {
        page_display_icon(self.icon.as_deref())
    }
}

impl From<&NotionPage> for NotionPageSummary {
    fn from(page: &NotionPage) -> Self {
        Self {
            id: page.id.clone(),
            workspace_id: page.workspace_id.clone(),
            parent_type: page.parent_type.clone(),
            parent_id: page.parent_id.clone(),
            title: page.title.clone(),
            icon: page.icon.clone(),
            url: page.url.clone(),
            archived: page.archived,
            is_favorite: page.is_favorite,
            properties_summary: page.properties_summary(),
            last_edited_time: page.last_edited_time.clone(),
        }
    }
}

fn page_display_title(title: &str) -> &str {
    let title = title.trim();
    if title.is_empty() {
        "Untitled"
    } else {
        title
    }
}

fn page_display_icon(icon: Option<&str>) -> &str {
    icon.filter(|s| !s.trim().is_empty()).unwrap_or("📄")
}

/// Notion Database metadata + property schema.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NotionDatabase {
    pub id: String,
    pub workspace_id: String,
    pub parent_type: String,
    pub parent_id: Option<String>,
    pub title: String,
    pub icon: Option<String>,
    pub description: String,
    pub schema: BTreeMap<String, NotionPropertySchema>,
    pub url: String,
    pub archived: bool,
    pub is_favorite: bool,
    pub last_edited_time: Option<String>,
    pub synced_at: Option<String>,
}

impl NotionDatabase {
    pub fn display_icon(&self) -> &str {
        self.icon
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("🗂")
    }

    pub fn display_title(&self) -> &str {
        let t = self.title.trim();
        if t.is_empty() {
            "Untitled Database"
        } else {
            t
        }
    }

    /// Finds the primary grouping property for Kanban Board view (`status` first, else first `select`).
    pub fn board_group_property(&self) -> Option<&NotionPropertySchema> {
        self.schema
            .values()
            .find(|s| s.prop_type == "status")
            .or_else(|| self.schema.values().find(|s| s.prop_type == "select"))
    }
}

/// Core Notion block variants faithfully covering Notion's block grammar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NotionBlockKind {
    Paragraph {
        rich_text: Vec<RichTextSpan>,
        color: NotionColor,
    },
    Heading1 {
        rich_text: Vec<RichTextSpan>,
        is_toggleable: bool,
        color: NotionColor,
    },
    Heading2 {
        rich_text: Vec<RichTextSpan>,
        is_toggleable: bool,
        color: NotionColor,
    },
    Heading3 {
        rich_text: Vec<RichTextSpan>,
        is_toggleable: bool,
        color: NotionColor,
    },
    BulletedListItem {
        rich_text: Vec<RichTextSpan>,
        color: NotionColor,
    },
    NumberedListItem {
        rich_text: Vec<RichTextSpan>,
        color: NotionColor,
    },
    ToDo {
        rich_text: Vec<RichTextSpan>,
        checked: bool,
        color: NotionColor,
    },
    Toggle {
        rich_text: Vec<RichTextSpan>,
        color: NotionColor,
    },
    Callout {
        rich_text: Vec<RichTextSpan>,
        icon: Option<String>,
        color: NotionColor,
    },
    Quote {
        rich_text: Vec<RichTextSpan>,
        color: NotionColor,
    },
    Divider,
    Code {
        rich_text: Vec<RichTextSpan>,
        language: String,
        caption: Vec<RichTextSpan>,
    },
    Table {
        table_width: usize,
        has_column_header: bool,
        has_row_header: bool,
    },
    TableRow {
        cells: Vec<Vec<RichTextSpan>>,
    },
    Image {
        url: String,
        caption: Vec<RichTextSpan>,
        is_hosted: bool,
    },
    Video {
        url: String,
        caption: Vec<RichTextSpan>,
        is_hosted: bool,
    },
    Audio {
        url: String,
        caption: Vec<RichTextSpan>,
        is_hosted: bool,
    },
    File {
        url: String,
        name: Option<String>,
        caption: Vec<RichTextSpan>,
    },
    Pdf {
        url: String,
        caption: Vec<RichTextSpan>,
    },
    Bookmark {
        url: String,
        caption: Vec<RichTextSpan>,
    },
    Embed {
        url: String,
        caption: Vec<RichTextSpan>,
    },
    LinkPreview {
        url: String,
    },
    ChildPage {
        title: String,
    },
    ChildDatabase {
        title: String,
    },
    ColumnList,
    Column,
    SyncedBlock {
        synced_from_block_id: Option<String>,
    },
    Equation {
        expression: String,
    },
    TableOfContents {
        color: NotionColor,
    },
    Button {
        rich_text: Vec<RichTextSpan>,
    },
    Unsupported {
        raw_type: String,
    },
}

impl Default for NotionBlockKind {
    fn default() -> Self {
        Self::Paragraph {
            rich_text: Vec::new(),
            color: NotionColor::Default,
        }
    }
}

/// A Notion block with optional recursively nested child blocks.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NotionBlock {
    pub id: String,
    pub page_id: String,
    pub parent_block_id: Option<String>,
    pub block_type: String,
    pub has_children: bool,
    pub archived: bool,
    pub kind: NotionBlockKind,
    pub children: Vec<NotionBlock>,
}

impl NotionBlock {
    pub fn plain_text(&self) -> String {
        let mut out = match &self.kind {
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
            | NotionBlockKind::Code { rich_text, .. } => spans_plain_text(rich_text),
            NotionBlockKind::TableRow { cells } => cells
                .iter()
                .map(|c| spans_plain_text(c))
                .collect::<Vec<_>>()
                .join(" | "),
            NotionBlockKind::Image { url, caption, .. }
            | NotionBlockKind::Video { url, caption, .. }
            | NotionBlockKind::Audio { url, caption, .. } => {
                let cap = spans_plain_text(caption);
                if cap.is_empty() {
                    url.clone()
                } else {
                    cap
                }
            }
            NotionBlockKind::File { url, name, caption } => {
                let cap = spans_plain_text(caption);
                if let Some(n) = name {
                    n.clone()
                } else if !cap.is_empty() {
                    cap
                } else {
                    url.clone()
                }
            }
            NotionBlockKind::Pdf { url, caption }
            | NotionBlockKind::Bookmark { url, caption }
            | NotionBlockKind::Embed { url, caption } => {
                let cap = spans_plain_text(caption);
                if cap.is_empty() {
                    url.clone()
                } else {
                    format!("{cap} ({url})")
                }
            }
            NotionBlockKind::LinkPreview { url } => url.clone(),
            NotionBlockKind::ChildPage { title } | NotionBlockKind::ChildDatabase { title } => {
                title.clone()
            }
            NotionBlockKind::Equation { expression } => expression.clone(),
            NotionBlockKind::Divider => "---".to_string(),
            NotionBlockKind::TableOfContents { .. } => "Table of Contents".to_string(),
            NotionBlockKind::Button { rich_text } => {
                let text = spans_plain_text(rich_text);
                if text.trim().is_empty() {
                    "Button".to_string()
                } else {
                    text
                }
            }
            NotionBlockKind::Table { .. }
            | NotionBlockKind::ColumnList
            | NotionBlockKind::Column
            | NotionBlockKind::SyncedBlock { .. } => String::new(),
            NotionBlockKind::Unsupported { raw_type } => format!("[{raw_type}]"),
        };

        for child in &self.children {
            let ct = child.plain_text();
            if !ct.trim().is_empty() {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&ct);
            }
        }
        out
    }
}

pub fn spans_plain_text(spans: &[RichTextSpan]) -> String {
    let mut out = String::new();
    for sp in spans {
        out.push_str(&sp.plain_text);
    }
    out
}

/// Notion comment on a page or discussion thread.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NotionComment {
    pub id: String,
    pub page_id: String,
    pub discussion_id: String,
    pub author: String,
    pub body: String,
    pub rich_text: Vec<RichTextSpan>,
    pub created_time: String,
}

/// Embedded image/file media reference inside a Notion page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotionMediaRef {
    pub block_id: String,
    pub url: String,
    pub display_name: String,
    pub mime_type: String,
    pub is_image: bool,
}

/// Complete page detail: metadata + block tree + comments + markdown + media refs.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NotionPageDetail {
    pub page: NotionPage,
    pub blocks: Vec<NotionBlock>,
    pub comments: Vec<NotionComment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comments_error: Option<String>,
    pub markdown: String,
    pub media: Vec<NotionMediaRef>,
}

impl NotionPageDetail {
    /// Compact detail: identifiers, Markdown body, what was left out, and how to get it.
    pub(crate) fn compact(
        &self,
        hint: String,
        comments_state: &str,
        comments_count: Option<usize>,
        comments_error: Option<&str>,
    ) -> Value {
        let fields: Vec<String> = serde_json::to_value(&self.page)
            .ok()
            .and_then(|v| v.as_object().cloned())
            .map(|map| {
                map.keys()
                    .filter(|k| !matches!(k.as_str(), "id" | "title" | "last_edited_time"))
                    .map(|k| format!("page.{k}"))
                    .collect()
            })
            .unwrap_or_default();
        let mut out = serde_json::Map::new();
        out.insert("id".into(), serde_json::json!(self.page.id));
        out.insert("title".into(), serde_json::json!(self.page.title));
        out.insert(
            "last_edited_time".into(),
            serde_json::json!(self.page.last_edited_time),
        );
        out.insert("markdown".into(), serde_json::json!(self.markdown));
        out.insert(
            "incomplete_blocks".into(),
            serde_json::json!(crate::parser::incomplete_block_ids(&self.blocks)),
        );
        out.insert("comments_state".into(), serde_json::json!(comments_state));
        if let Some(err) = comments_error {
            out.insert("comments_error".into(), serde_json::json!(err));
        }
        out.insert(
            "omitted".into(),
            serde_json::json!({
                "blocks": self.blocks.len(),
                "comments": comments_count,
                "media": self.media.len(),
                "fields": fields,
            }),
        );
        out.insert("hint".into(), serde_json::json!(hint));
        Value::Object(out)
    }

    /// Full detail plus the computed `incomplete_blocks` and the `comments_has_more` keys.
    /// Serializes at most `DETAIL_COMMENTS_LIMIT` (2,000) comments without cloning so any
    /// 2,001st lookahead entry remains internal/cached.
    pub(crate) fn full_json(&self, comments_has_more: bool) -> Value {
        #[derive(Serialize)]
        struct FullPageDetailView<'a> {
            page: &'a NotionPage,
            blocks: &'a [NotionBlock],
            comments: &'a [NotionComment],
            #[serde(skip_serializing_if = "Option::is_none")]
            comments_error: &'a Option<String>,
            markdown: &'a str,
            media: &'a [NotionMediaRef],
            incomplete_blocks: Vec<String>,
            comments_has_more: bool,
        }

        let visible_len = self.comments.len().min(crate::notion::DETAIL_COMMENTS_LIMIT);
        let view = FullPageDetailView {
            page: &self.page,
            blocks: &self.blocks,
            comments: &self.comments[..visible_len],
            comments_error: &self.comments_error,
            markdown: &self.markdown,
            media: &self.media,
            incomplete_blocks: crate::parser::incomplete_block_ids(&self.blocks),
            comments_has_more,
        };
        serde_json::to_value(view).unwrap_or_else(|_| serde_json::json!({}))
    }
}

/// Search hit returned by SQLite FTS5 / LIKE fallback or remote Notion search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotionSearchHit {
    pub id: String,
    pub workspace_id: String,
    /// `"page"` or `"database"`.
    pub item_type: String,
    pub title: String,
    pub icon: Option<String>,
    pub snippet: String,
    pub url: String,
    pub last_edited_time: Option<String>,
}

/// Result returned by `notion download` CLI and `download_media` MCP tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadedMedia {
    pub ok: bool,
    pub path: String,
    pub file_name: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub is_image: bool,
}

/// Inline image staging metadata (used by composer clipboard paste).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineImageUpload {
    pub temp_id: String,
    pub mime_type: String,
    pub bytes: Vec<u8>,
    pub file_name: Option<String>,
}

/// Database view mode in the Notion TUI (`1 Table`, `2 Board`, `3 List`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DatabaseViewMode {
    #[default]
    Table,
    Board,
    List,
}

impl DatabaseViewMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Table => "Table",
            Self::Board => "Board",
            Self::List => "List",
        }
    }
}

/// Database sort mode in TUI (`s` cycles).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseSortMode {
    #[default]
    LastEditedDesc,
    LastEditedAsc,
    TitleAsc,
    TitleDesc,
    StatusAsc,
}

impl DatabaseSortMode {
    pub fn next(self) -> Self {
        match self {
            Self::LastEditedDesc => Self::LastEditedAsc,
            Self::LastEditedAsc => Self::TitleAsc,
            Self::TitleAsc => Self::TitleDesc,
            Self::TitleDesc => Self::StatusAsc,
            Self::StatusAsc => Self::LastEditedDesc,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::LastEditedDesc => "Edited ↓",
            Self::LastEditedAsc => "Edited ↑",
            Self::TitleAsc => "Title A-Z",
            Self::TitleDesc => "Title Z-A",
            Self::StatusAsc => "Status",
        }
    }
}

/// Events published by the background sync engine / IPC daemon to the TUI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum CoreEvent {
    ProfileLoaded(NotionUser),
    WorkspaceTreeUpdated {
        workspace_id: String,
        pages: Vec<NotionPageSummary>,
        databases: Vec<NotionDatabase>,
    },
    PageDetailUpdated(Box<NotionPageDetail>),
    DatabaseRowsUpdated {
        database_id: String,
        database: Option<NotionDatabase>,
        rows: Vec<NotionPage>,
    },
    CommentsUpdated {
        page_id: String,
        comments: Vec<NotionComment>,
    },
    SyncStateChanged {
        busy: bool,
        summary: String,
    },
    SyncError(String),
}

/// Result of checking or applying a GitHub Release self-update.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateCheckResult {
    pub current_version: String,
    pub latest_version: String,
    pub update_available: bool,
    pub release_url: String,
    pub asset_name: Option<String>,
    pub asset_size: Option<u64>,
    pub updated: bool,
    pub binary_path: PathBuf,
}

/// Normalizes a Notion UUID (with or without dashes) into canonical hyphenated lowercase form when 32 hex chars.
pub fn normalize_notion_id(raw: &str) -> String {
    let trimmed = raw.trim();
    let hex_only: String = trimmed
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect::<String>()
        .to_ascii_lowercase();
    if hex_only.len() == 32 && trimmed.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        format!(
            "{}-{}-{}-{}-{}",
            &hex_only[0..8],
            &hex_only[8..12],
            &hex_only[12..16],
            &hex_only[16..20],
            &hex_only[20..32]
        )
    } else {
        trimmed.to_string()
    }
}

/// Extracts a Notion 32-hex page/database ID from a full `notion.so` URL if provided.
pub fn extract_id_from_notion_url(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if !trimmed.contains("notion.so") && !trimmed.contains("notion.site") {
        return None;
    }
    let no_query = trimmed.split('?').next().unwrap_or(trimmed);
    let no_frag = no_query.split('#').next().unwrap_or(no_query);
    let last_seg = no_frag.rsplit('/').next().unwrap_or("");
    let candidate = last_seg.rsplit('-').next().unwrap_or(last_seg);
    let hex_only: String = candidate
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect::<String>()
        .to_ascii_lowercase();
    if hex_only.len() == 32 {
        Some(normalize_notion_id(&hex_only))
    } else {
        None
    }
}

/// Compares two byte strings without stopping at the first difference (for secrets and API keys).
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Helper to build a Notion API rich_text array payload from plain text.
pub fn plain_rich_text_json(text: &str) -> Value {
    serde_json::json!([{
        "type": "text",
        "text": { "content": text }
    }])
}

/// Structured application error carrying a machine-readable code and optional details.
#[derive(Debug, Clone)]
pub struct CodedError {
    pub code: &'static str,
    pub message: String,
    pub details: Option<Value>,
}

impl std::fmt::Display for CodedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CodedError {}

pub fn coded(code: &'static str, msg: impl Into<String>) -> anyhow::Error {
    CodedError {
        code,
        message: msg.into(),
        details: None,
    }
    .into()
}

pub fn coded_with_details(
    code: &'static str,
    msg: impl Into<String>,
    details: Value,
) -> anyhow::Error {
    CodedError {
        code,
        message: msg.into(),
        details: Some(details),
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_and_extract_notion_ids() {
        assert_eq!(
            normalize_notion_id("1234567890abcdef1234567890abcdef"),
            "12345678-90ab-cdef-1234-567890abcdef"
        );
        assert_eq!(
            normalize_notion_id("12345678-90AB-CDEF-1234-567890ABCDEF"),
            "12345678-90ab-cdef-1234-567890abcdef"
        );
        assert_eq!(
            extract_id_from_notion_url(
                "https://www.notion.so/acme/Project-Spec-1234567890abcdef1234567890abcdef?pvs=4"
            ),
            Some("12345678-90ab-cdef-1234-567890abcdef".to_string())
        );
    }

    #[test]
    fn test_constant_time_eq() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secret1"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn test_notion_color_parsing() {
        assert_eq!(NotionColor::from_notion_str(Some("blue")), NotionColor::Blue);
        assert_eq!(
            NotionColor::from_notion_str(Some("yellow_background")),
            NotionColor::Yellow
        );
        assert_eq!(NotionColor::from_notion_str(None), NotionColor::Default);
    }

    #[test]
    fn compact_lists_omissions_and_hint() {
        let block = |id: &str| NotionBlock {
            id: id.into(),
            block_type: "paragraph".into(),
            ..Default::default()
        };
        let comment = |id: &str| NotionComment {
            id: id.into(),
            page_id: "p1".into(),
            body: "ignored".into(),
            ..Default::default()
        };
        let detail = NotionPageDetail {
            page: NotionPage {
                id: "p1".into(),
                title: "Spec".into(),
                last_edited_time: Some("2026-11-01T00:00:00.000Z".into()),
                ..Default::default()
            },
            blocks: vec![block("b1"), block("b2")],
            comments: vec![comment("c1"), comment("c2"), comment("c3")],
            comments_error: Some("ignored".into()),
            markdown: "hello".into(),
            media: Vec::new(),
        };

        let cached = detail.compact("H".into(), "cached", Some(1), None);
        assert_eq!(cached["omitted"]["blocks"], 2);
        assert_eq!(cached["omitted"]["comments"], 1);
        assert_eq!(cached["omitted"]["media"], 0);
        assert_eq!(cached["comments_state"], "cached");
        assert!(cached.get("comments_error").is_none());
        let fields: Vec<&str> = cached["omitted"]["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(fields.contains(&"page.properties"));
        assert!(!fields.contains(&"page.id"));
        assert_eq!(cached["hint"], "H");

        let failed = detail.compact("H".into(), "failed", None, Some("E"));
        assert!(failed["omitted"]["comments"].is_null());
        assert_eq!(failed["comments_state"], "failed");
        assert_eq!(failed["comments_error"], "E");

        let not_fetched = detail.compact("H".into(), "not_fetched", None, None);
        assert!(not_fetched["omitted"]["comments"].is_null());
        assert_eq!(not_fetched["comments_state"], "not_fetched");
        assert!(not_fetched.get("comments_error").is_none());
        assert!(not_fetched.get("comments_has_more").is_none());
    }

    #[test]
    fn full_json_caps_serialized_comments_at_detail_limit_without_mutating_detail() {
        let make_comments = |n: usize| -> Vec<NotionComment> {
            (0..n)
                .map(|i| NotionComment {
                    id: format!("c-{i}"),
                    page_id: "p1".into(),
                    body: format!("body-{i}"),
                    ..Default::default()
                })
                .collect()
        };

        let exact_detail = NotionPageDetail {
            page: NotionPage {
                id: "p1".into(),
                title: "Exact 2000".into(),
                ..Default::default()
            },
            comments: make_comments(crate::notion::DETAIL_COMMENTS_LIMIT),
            ..Default::default()
        };
        let exact_json = exact_detail.full_json(false);
        assert_eq!(
            exact_json["comments"].as_array().unwrap().len(),
            crate::notion::DETAIL_COMMENTS_LIMIT
        );
        assert_eq!(exact_json["comments_has_more"], false);
        assert_eq!(exact_json["incomplete_blocks"], serde_json::json!([]));
        assert!(exact_json.get("comments_error").is_none());

        let partial_detail = NotionPageDetail {
            page: NotionPage {
                id: "p2".into(),
                title: "Partial 2001".into(),
                ..Default::default()
            },
            comments: make_comments(crate::notion::DETAIL_COMMENTS_LIMIT + 1),
            ..Default::default()
        };
        let partial_json = partial_detail.full_json(true);
        let serialized = partial_json["comments"].as_array().unwrap();
        assert_eq!(serialized.len(), crate::notion::DETAIL_COMMENTS_LIMIT);
        assert_eq!(serialized.first().unwrap()["id"], "c-0");
        assert_eq!(serialized.last().unwrap()["id"], "c-1999");
        assert_eq!(partial_json["comments_has_more"], true);
        // The 2,001st lookahead entry remains intact on the in-memory detail.
        assert_eq!(
            partial_detail.comments.len(),
            crate::notion::DETAIL_COMMENTS_LIMIT + 1
        );
        assert_eq!(partial_detail.comments[2000].id, "c-2000");
    }
}
