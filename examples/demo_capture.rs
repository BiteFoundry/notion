//! Captures 8 real `ratatui::TestBackend` frames (120x34) from `notion_cli::ui::draw`
//! with 100% fictional English demo data and invokes the shared studio-grade GIF renderer.
//!
//! The renderer is not part of this repository. Point the example at it with:
//! - `DEMO_RENDERER_SCRIPT`: path to `render_tui_gif.py`
//!   (default `<repo>/scripts/render_tui_gif.py`)
//! - `DEMO_PREVIEW_DIR`: where preview PNGs go (default `<repo>/target/demo-previews/notion`)

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
};

use notion_cli::{
    auth::AuthManager,
    config::{AppConfig, DensityMode, PageWidthMode, ThemePreset, WorkspaceProfile},
    models::{
        DatabaseSortMode, DatabaseViewMode, NotionBlock, NotionBlockKind, NotionColor,
        NotionComment, NotionDatabase, NotionPage, NotionPageDetail, NotionPropertySchema,
        NotionPropertyValue, NotionUser, RichTextAnnotations, RichTextSpan, SelectOption,
    },
    notion::NotionClient,
    storage::Storage,
    sync::{FocusTarget, SyncCommand, SyncController},
    ui::{
        self,
        app::{AppState, CanvasView, Focus, NavTarget, UiState},
        overlays::{command_palette::CommandPalette, Overlay},
        theme::Theme,
    },
};
use ratatui::{
    backend::TestBackend,
    buffer::Buffer,
    style::{Color, Modifier},
    Terminal,
};
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};

const WIDTH: u16 = 120;
const HEIGHT: u16 = 34;

/// A non-empty environment variable as a path.
fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// `DEMO_RENDERER_SCRIPT`, else `<repo>/scripts/render_tui_gif.py`.
fn renderer_script() -> PathBuf {
    env_path("DEMO_RENDERER_SCRIPT").unwrap_or_else(|| {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("scripts")
            .join("render_tui_gif.py")
    })
}

/// `DEMO_PREVIEW_DIR`, else `<repo>/target/demo-previews/notion`; created if missing.
fn preview_dir() -> PathBuf {
    let dir = env_path("DEMO_PREVIEW_DIR").unwrap_or_else(|| {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("demo-previews")
            .join("notion")
    });
    std::fs::create_dir_all(&dir)
        .unwrap_or_else(|err| panic!("create preview dir {}: {err}", dir.display()));
    dir
}

fn color_to_json(c: Color) -> Value {
    match c {
        Color::Reset => Value::Null,
        Color::Rgb(r, g, b) => json!([r, g, b]),
        Color::Black => json!("Black"),
        Color::Red => json!("Red"),
        Color::Green => json!("Green"),
        Color::Yellow => json!("Yellow"),
        Color::Blue => json!("Blue"),
        Color::Magenta => json!("Magenta"),
        Color::Cyan => json!("Cyan"),
        Color::Gray => json!("Gray"),
        Color::DarkGray => json!("DarkGray"),
        Color::LightRed => json!("LightRed"),
        Color::LightGreen => json!("LightGreen"),
        Color::LightYellow => json!("LightYellow"),
        Color::LightBlue => json!("LightBlue"),
        Color::LightMagenta => json!("LightMagenta"),
        Color::LightCyan => json!("LightCyan"),
        Color::White => json!("White"),
        Color::Indexed(idx) => {
            let (r, g, b) = xterm256_to_rgb(idx);
            json!([r, g, b])
        }
    }
}

fn xterm256_to_rgb(idx: u8) -> (u8, u8, u8) {
    if idx < 16 {
        const ANSI16: [(u8, u8, u8); 16] = [
            (24, 26, 33),
            (247, 118, 142),
            (158, 206, 106),
            (224, 175, 104),
            (122, 162, 247),
            (187, 154, 247),
            (125, 207, 255),
            (169, 177, 214),
            (86, 95, 137),
            (255, 140, 160),
            (175, 224, 128),
            (240, 200, 130),
            (148, 184, 255),
            (205, 176, 255),
            (155, 222, 255),
            (240, 244, 252),
        ];
        return ANSI16[idx as usize];
    }
    if idx >= 232 {
        let v = 8 + (idx - 232) * 10;
        return (v, v, v);
    }
    let n = idx - 16;
    let r = n / 36;
    let g = (n % 36) / 6;
    let b = n % 6;
    let map = |c: u8| if c == 0 { 0 } else { 55 + c * 40 };
    (map(r), map(g), map(b))
}

fn modifier_flags(m: Modifier) -> u8 {
    let mut flags = 0u8;
    if m.contains(Modifier::BOLD) {
        flags |= 1;
    }
    if m.contains(Modifier::ITALIC) {
        flags |= 2;
    }
    if m.contains(Modifier::DIM) {
        flags |= 4;
    }
    if m.contains(Modifier::UNDERLINED) {
        flags |= 8;
    }
    if m.contains(Modifier::REVERSED) {
        flags |= 16;
    }
    flags
}

fn cell_single_char(symbol: &str, is_bold: bool) -> char {
    let ch = symbol
        .chars()
        .find(|c| *c != '\u{fe0e}' && *c != '\u{fe0f}' && *c != '\u{200d}')
        .unwrap_or(' ');
    match ch {
        '📄' | '📝' => '▪',
        '🗂' | '📋' => '■',
        '💡' | '✦' | '⚡' => '●',
        '🚀' | '🎯' => '▸',
        '💬' | '◈' => '●',
        '🔗' => '→',
        '📎' => '+',
        '🔍' | '⌕' => '/',
        'ℹ' => 'i',
        '★' | '☆' => '*',
        '☐' => '▫',
        '☑' => '■',
        '▎' | '▏' => '▌',
        '✓' | '✔' | '✅' if is_bold => '▪',
        '◆' if is_bold => '▪',
        '🕒' | '🗑' => '·',
        other => other,
    }
}

fn buffer_to_rows(buf: &Buffer, width: u16, height: u16) -> Vec<Value> {
    let mut rows = Vec::with_capacity(height as usize);
    for y in 0..height {
        let mut runs: Vec<Value> = Vec::new();
        let mut cur_text = String::new();
        let mut cur_style: Option<(Color, Color, u8)> = None;

        for x in 0..width {
            let cell = &buf[(x, y)];
            let flags = modifier_flags(cell.modifier);
            let ch = cell_single_char(cell.symbol(), (flags & 1) != 0);
            let style_key = (cell.fg, cell.bg, flags);
            if cur_style == Some(style_key) {
                cur_text.push(ch);
            } else {
                if let Some((fg, bg, fl)) = cur_style {
                    runs.push(json!([cur_text, color_to_json(fg), color_to_json(bg), fl]));
                    cur_text = String::new();
                }
                cur_text.push(ch);
                cur_style = Some(style_key);
            }
        }
        if let Some((fg, bg, fl)) = cur_style {
            runs.push(json!([cur_text, color_to_json(fg), color_to_json(bg), fl]));
        }
        rows.push(Value::Array(runs));
    }
    rows
}

fn capture_frame(
    app: &mut AppState,
    ui: &mut UiState,
    duration_ms: u64,
    caption: &str,
    key_badge: &str,
) -> Value {
    let backend = TestBackend::new(WIDTH, HEIGHT);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|f| {
            let _ = ui::draw(f, app, ui);
        })
        .expect("draw frame");
    let rows = buffer_to_rows(terminal.backend().buffer(), WIDTH, HEIGHT);
    json!({
        "duration_ms": duration_ms,
        "caption": caption,
        "key_badge": key_badge,
        "rows": rows,
    })
}

fn rt(text: &str) -> RichTextSpan {
    RichTextSpan::plain(text)
}

fn rt_bold(text: &str) -> RichTextSpan {
    RichTextSpan {
        plain_text: text.into(),
        annotations: RichTextAnnotations {
            bold: true,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn rt_code(text: &str) -> RichTextSpan {
    RichTextSpan {
        plain_text: text.into(),
        annotations: RichTextAnnotations {
            code: true,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn opt(name: &str, color: NotionColor) -> SelectOption {
    SelectOption {
        id: Some(name.to_ascii_lowercase().replace(' ', "_")),
        name: name.into(),
        color,
    }
}

fn make_db_row(
    ws_id: &str,
    db_id: &str,
    id: &str,
    icon: &str,
    title: &str,
    status: (&str, NotionColor),
    priority: (&str, NotionColor),
    tags: &[(&str, NotionColor)],
    owner: &str,
    edited: &str,
) -> NotionPage {
    let mut props = BTreeMap::new();
    props.insert("Name".into(), NotionPropertyValue::Title(title.into()));
    props.insert(
        "Status".into(),
        NotionPropertyValue::Status(Some(opt(status.0, status.1))),
    );
    props.insert(
        "Priority".into(),
        NotionPropertyValue::Select(Some(opt(priority.0, priority.1))),
    );
    props.insert(
        "Tags".into(),
        NotionPropertyValue::MultiSelect(tags.iter().map(|(n, c)| opt(n, *c)).collect()),
    );
    props.insert("Owner".into(), NotionPropertyValue::People(vec![owner.into()]));

    NotionPage {
        id: id.into(),
        workspace_id: ws_id.into(),
        parent_type: "database_id".into(),
        parent_id: Some(db_id.into()),
        title: title.into(),
        icon: Some(icon.into()),
        cover_url: None,
        url: format!("https://www.notion.so/acme/{id}"),
        archived: false,
        is_favorite: false,
        properties: props,
        plain_text: format!("{title} {} {}", status.0, priority.0),
        created_time: Some("2025-11-01T09:00:00Z".into()),
        last_edited_time: Some(edited.into()),
        created_by: None,
        last_edited_by: None,
        synced_at: Some("2025-11-19T14:30:00Z".into()),
    }
}

fn build_demo_state() -> (AppState, UiState) {
    let ws_id = "ws-acme-hq";
    let cfg = AppConfig {
        active_workspace: "acme-hq".into(),
        workspaces: vec![
            WorkspaceProfile {
                name: "acme-hq".into(),
                workspace_id: ws_id.into(),
                workspace_name: "Acme Product HQ".into(),
                workspace_icon: Some("▪".into()),
                bot_id: Some("bot-acme".into()),
                bot_name: Some("Acme Workspace Bot".into()),
                auth_mode: "integration".into(),
                api_base_url: None,
            },
            WorkspaceProfile {
                name: "acme-eng".into(),
                workspace_id: "ws-acme-eng".into(),
                workspace_name: "Acme Engineering".into(),
                workspace_icon: Some("▪".into()),
                bot_id: Some("bot-eng".into()),
                bot_name: Some("Eng Sync Bot".into()),
                auth_mode: "oauth".into(),
                api_base_url: None,
            },
        ],
        theme: ThemePreset::CleanDark,
        density: DensityMode::Cozy,
        page_width: PageWidthMode::Centered,
        show_sidebar: true,
        inline_images: true,
        ..Default::default()
    };

    let auth = AuthManager::from_token(cfg.clone(), "demo-placeholder-token");
    let client = NotionClient::new(&cfg, auth.clone()).expect("client");
    let storage = Storage::open_in_memory().expect("in-memory sqlite");
    let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
    let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<SyncCommand>();
    let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);
    let (ui_tx, _ui_rx) = mpsc::unbounded_channel();

    let mut app = AppState::new(cfg.clone(), auth, client, storage, sync_ctrl, ui_tx);
    app.workspace_id = ws_id.into();
    app.workspace_name = "Acme Product HQ".into();
    app.current_user = Some(NotionUser {
        id: "u-alex".into(),
        name: "Alex Rivera".into(),
        user_type: "person".into(),
        avatar_url: None,
        email: Some("alex.rivera@acme.example".into()),
        workspace_name: Some("Acme Product HQ".into()),
    });

    let mut ui = UiState::from_config(&cfg);
    ui.theme = Theme::clean_dark();
    ui.focus = Focus::Canvas;

    // Pages hierarchy
    let page_specs = NotionPage {
        id: "page-specs".into(),
        workspace_id: ws_id.into(),
        parent_type: "workspace".into(),
        parent_id: None,
        title: "Product Specs".into(),
        icon: Some("▪".into()),
        url: "https://www.notion.so/acme/product-specs".into(),
        is_favorite: false,
        last_edited_time: Some("2025-11-18T10:00:00Z".into()),
        ..Default::default()
    };

    let mut launch_props = BTreeMap::new();
    launch_props.insert(
        "Status".into(),
        NotionPropertyValue::Status(Some(opt("In Progress", NotionColor::Blue))),
    );
    launch_props.insert(
        "Priority".into(),
        NotionPropertyValue::Select(Some(opt("P0", NotionColor::Red))),
    );
    launch_props.insert(
        "Tags".into(),
        NotionPropertyValue::MultiSelect(vec![
            opt("Core", NotionColor::Purple),
            opt("Q4", NotionColor::Green),
        ]),
    );
    launch_props.insert(
        "Owner".into(),
        NotionPropertyValue::People(vec!["Alex Rivera".into()]),
    );

    let page_launch = NotionPage {
        id: "page-q4-launch".into(),
        workspace_id: ws_id.into(),
        parent_type: "page_id".into(),
        parent_id: Some("page-specs".into()),
        title: "Q4 Launch Plan".into(),
        icon: Some("▸".into()),
        url: "https://www.notion.so/acme/q4-launch-plan".into(),
        is_favorite: true,
        properties: launch_props,
        plain_text: "Q4 Launch Plan release readiness checklist canary SLOs rollback".into(),
        created_time: Some("2025-11-05T09:00:00Z".into()),
        last_edited_time: Some("2025-11-19T14:25:00Z".into()),
        synced_at: Some("2025-11-19T14:30:00Z".into()),
        ..Default::default()
    };

    let page_arch = NotionPage {
        id: "page-arch".into(),
        workspace_id: ws_id.into(),
        parent_type: "page_id".into(),
        parent_id: Some("page-specs".into()),
        title: "API Architecture".into(),
        icon: Some("▪".into()),
        url: "https://www.notion.so/acme/api-architecture".into(),
        last_edited_time: Some("2025-11-17T16:40:00Z".into()),
        plain_text: "REST v1 API design cursor pagination WAL FTS5 indexing".into(),
        ..Default::default()
    };

    let page_design = NotionPage {
        id: "page-design".into(),
        workspace_id: ws_id.into(),
        parent_type: "workspace".into(),
        parent_id: None,
        title: "Design System".into(),
        icon: Some("▪".into()),
        url: "https://www.notion.so/acme/design-system".into(),
        last_edited_time: Some("2025-11-16T11:20:00Z".into()),
        plain_text: "Editorial typography 10 canonical Notion colors minimal chrome".into(),
        ..Default::default()
    };

    let page_notes = NotionPage {
        id: "page-notes".into(),
        workspace_id: ws_id.into(),
        parent_type: "workspace".into(),
        parent_id: None,
        title: "Weekly Sync Notes".into(),
        icon: Some("▪".into()),
        url: "https://www.notion.so/acme/weekly-sync-notes".into(),
        last_edited_time: Some("2025-11-19T08:15:00Z".into()),
        plain_text: "Weekly engineering sync metrics milestones roadmap review".into(),
        ..Default::default()
    };

    // Blocks for Q4 Launch Plan
    let blocks = vec![
        NotionBlock {
            id: "blk-callout".into(),
            page_id: "page-q4-launch".into(),
            block_type: "callout".into(),
            kind: NotionBlockKind::Callout {
                rich_text: vec![
                    rt_bold("Launch Gate: "),
                    rt("All P0 milestones must pass <35ms p99 latency & zero-warning CI."),
                ],
                icon: Some("●".into()),
                color: NotionColor::Yellow,
            },
            ..Default::default()
        },
        NotionBlock {
            id: "blk-h2-checks".into(),
            page_id: "page-q4-launch".into(),
            block_type: "heading_2".into(),
            kind: NotionBlockKind::Heading2 {
                rich_text: vec![rt("Release Readiness Checklist")],
                is_toggleable: false,
                color: NotionColor::Default,
            },
            ..Default::default()
        },
        NotionBlock {
            id: "blk-todo-1".into(),
            page_id: "page-q4-launch".into(),
            block_type: "to_do".into(),
            kind: NotionBlockKind::ToDo {
                rich_text: vec![rt("Ship SQLite WAL + FTS5 incremental workspace sync")],
                checked: true,
                color: NotionColor::Default,
            },
            ..Default::default()
        },
        NotionBlock {
            id: "blk-todo-2".into(),
            page_id: "page-q4-launch".into(),
            block_type: "to_do".into(),
            kind: NotionBlockKind::ToDo {
                rich_text: vec![rt("Finalize multi-region Canary SLO dashboards & alerts")],
                checked: false,
                color: NotionColor::Default,
            },
            ..Default::default()
        },
        NotionBlock {
            id: "blk-todo-3".into(),
            page_id: "page-q4-launch".into(),
            block_type: "to_do".into(),
            kind: NotionBlockKind::ToDo {
                rich_text: vec![rt("Publish cross-platform npm binaries & MCP docs")],
                checked: false,
                color: NotionColor::Default,
            },
            ..Default::default()
        },
        NotionBlock {
            id: "blk-toggle-1".into(),
            page_id: "page-q4-launch".into(),
            block_type: "toggle".into(),
            has_children: true,
            kind: NotionBlockKind::Toggle {
                rich_text: vec![rt_bold("Rollout Sequence & Automated Rollback Steps")],
                color: NotionColor::Default,
            },
            children: vec![
                NotionBlock {
                    id: "blk-toggle-c1".into(),
                    page_id: "page-q4-launch".into(),
                    parent_block_id: Some("blk-toggle-1".into()),
                    block_type: "bulleted_list_item".into(),
                    kind: NotionBlockKind::BulletedListItem {
                        rich_text: vec![
                            rt("Stage 1: Route "),
                            rt_code("25%"),
                            rt(" canary traffic to "),
                            rt_code("eu-west-1"),
                            rt(" for 2 hours."),
                        ],
                        color: NotionColor::Default,
                    },
                    ..Default::default()
                },
                NotionBlock {
                    id: "blk-toggle-c2".into(),
                    page_id: "page-q4-launch".into(),
                    parent_block_id: Some("blk-toggle-1".into()),
                    block_type: "bulleted_list_item".into(),
                    kind: NotionBlockKind::BulletedListItem {
                        rich_text: vec![
                            rt("Stage 2: Verify zero 5xx spikes & auto-revert if error rate > "),
                            rt_bold("0.1%"),
                            rt("."),
                        ],
                        color: NotionColor::Default,
                    },
                    ..Default::default()
                },
            ],
            ..Default::default()
        },
        NotionBlock {
            id: "blk-code".into(),
            page_id: "page-q4-launch".into(),
            block_type: "code".into(),
            kind: NotionBlockKind::Code {
                rich_text: vec![rt(
                    "notion db query \"Q4 Roadmap\" --status \"In Progress\" --json\nnotion pages append \"Q4 Launch Plan\" \"/todo Tag v1.0 release\"",
                )],
                language: "bash".into(),
                caption: vec![],
            },
            ..Default::default()
        },
        NotionBlock {
            id: "blk-table".into(),
            page_id: "page-q4-launch".into(),
            block_type: "table".into(),
            has_children: true,
            kind: NotionBlockKind::Table {
                table_width: 3,
                has_column_header: true,
                has_row_header: false,
            },
            children: vec![
                NotionBlock {
                    id: "blk-tr-0".into(),
                    page_id: "page-q4-launch".into(),
                    block_type: "table_row".into(),
                    kind: NotionBlockKind::TableRow {
                        cells: vec![vec![rt("Milestone")], vec![rt("Target")], vec![rt("Owner")]],
                    },
                    ..Default::default()
                },
                NotionBlock {
                    id: "blk-tr-1".into(),
                    page_id: "page-q4-launch".into(),
                    block_type: "table_row".into(),
                    kind: NotionBlockKind::TableRow {
                        cells: vec![
                            vec![rt("Canary Ramp (25%)")],
                            vec![rt("Nov 20 · <35ms p99")],
                            vec![rt("Maya Chen")],
                        ],
                    },
                    ..Default::default()
                },
                NotionBlock {
                    id: "blk-tr-2".into(),
                    page_id: "page-q4-launch".into(),
                    block_type: "table_row".into(),
                    kind: NotionBlockKind::TableRow {
                        cells: vec![
                            vec![rt("v1.0 General Release")],
                            vec![rt("Nov 24 · 6 npm targets")],
                            vec![rt("Alex Rivera")],
                        ],
                    },
                    ..Default::default()
                },
            ],
            ..Default::default()
        },
    ];

    let comments = vec![
        NotionComment {
            id: "cm-1".into(),
            page_id: "page-q4-launch".into(),
            discussion_id: "disc-1".into(),
            author: "Maya Chen".into(),
            body: "Canary latency held at 19ms p99 overnight. Ready to check off the SLO task!".into(),
            rich_text: vec![rt(
                "Canary latency held at 19ms p99 overnight. Ready to check off the SLO task!",
            )],
            created_time: "2025-11-19T11:40:00Z".into(),
        },
        NotionComment {
            id: "cm-2".into(),
            page_id: "page-q4-launch".into(),
            discussion_id: "disc-1".into(),
            author: "Liam Brooks".into(),
            body: "Verified all 6 npm platform packages and MCP tool schemas on staging.".into(),
            rich_text: vec![rt(
                "Verified all 6 npm platform packages and MCP tool schemas on staging.",
            )],
            created_time: "2025-11-19T13:15:00Z".into(),
        },
        NotionComment {
            id: "cm-3".into(),
            page_id: "page-q4-launch".into(),
            discussion_id: "disc-2".into(),
            author: "Priya Nair".into(),
            body: "Rollback drill completed in 14 seconds. Green light for Q4 launch.".into(),
            rich_text: vec![rt(
                "Rollback drill completed in 14 seconds. Green light for Q4 launch.",
            )],
            created_time: "2025-11-19T14:05:00Z".into(),
        },
    ];

    let page_detail = NotionPageDetail {
        page: page_launch.clone(),
        blocks,
        comments,
        comments_error: None,
        markdown: String::new(),
        media: vec![],
    };

    // Q4 Roadmap Database
    let db_id = "db-q4-roadmap";
    let mut schema = BTreeMap::new();
    schema.insert(
        "Name".into(),
        NotionPropertySchema {
            id: "title".into(),
            name: "Name".into(),
            prop_type: "title".into(),
            options: vec![],
            relation_database_id: None,
        },
    );
    schema.insert(
        "Status".into(),
        NotionPropertySchema {
            id: "status".into(),
            name: "Status".into(),
            prop_type: "status".into(),
            options: vec![
                opt("Not Started", NotionColor::Gray),
                opt("In Progress", NotionColor::Blue),
                opt("In Review", NotionColor::Purple),
                opt("Done", NotionColor::Green),
            ],
            relation_database_id: None,
        },
    );
    schema.insert(
        "Priority".into(),
        NotionPropertySchema {
            id: "priority".into(),
            name: "Priority".into(),
            prop_type: "select".into(),
            options: vec![
                opt("P0 High", NotionColor::Red),
                opt("P1 Med", NotionColor::Orange),
                opt("P2 Low", NotionColor::Yellow),
            ],
            relation_database_id: None,
        },
    );
    schema.insert(
        "Tags".into(),
        NotionPropertySchema {
            id: "tags".into(),
            name: "Tags".into(),
            prop_type: "multi_select".into(),
            options: vec![
                opt("Core", NotionColor::Purple),
                opt("TUI", NotionColor::Blue),
                opt("API", NotionColor::Green),
                opt("MCP", NotionColor::Pink),
            ],
            relation_database_id: None,
        },
    );
    schema.insert(
        "Owner".into(),
        NotionPropertySchema {
            id: "owner".into(),
            name: "Owner".into(),
            prop_type: "people".into(),
            options: vec![],
            relation_database_id: None,
        },
    );

    let db_roadmap = NotionDatabase {
        id: db_id.into(),
        workspace_id: ws_id.into(),
        parent_type: "workspace".into(),
        parent_id: None,
        title: "Q4 Roadmap".into(),
        icon: Some("■".into()),
        description: "Acme Product HQ milestones — press 1 Table · 2 Board · 3 List".into(),
        schema,
        url: "https://www.notion.so/acme/q4-roadmap".into(),
        archived: false,
        is_favorite: true,
        last_edited_time: Some("2025-11-19T14:20:00Z".into()),
        synced_at: Some("2025-11-19T14:30:00Z".into()),
    };

    let db_feedback = NotionDatabase {
        id: "db-feedback".into(),
        workspace_id: ws_id.into(),
        parent_type: "workspace".into(),
        parent_id: None,
        title: "Customer Feedback".into(),
        icon: Some("■".into()),
        description: "Enterprise design partner triage & feature requests".into(),
        schema: BTreeMap::new(),
        url: "https://www.notion.so/acme/customer-feedback".into(),
        archived: false,
        is_favorite: false,
        last_edited_time: Some("2025-11-18T09:10:00Z".into()),
        synced_at: Some("2025-11-19T14:30:00Z".into()),
    };

    let db_rows = vec![
        make_db_row(
            ws_id,
            db_id,
            "row-1",
            "▸",
            "Editorial Page Canvas & Toggles",
            ("In Progress", NotionColor::Blue),
            ("P0 High", NotionColor::Red),
            &[("TUI", NotionColor::Blue), ("Core", NotionColor::Purple)],
            "Alex Rivera",
            "2025-11-19T14:20:00Z",
        ),
        make_db_row(
            ws_id,
            db_id,
            "row-2",
            "▪",
            "Database Table / Board / List",
            ("In Review", NotionColor::Purple),
            ("P0 High", NotionColor::Red),
            &[("TUI", NotionColor::Blue)],
            "Maya Chen",
            "2025-11-19T13:50:00Z",
        ),
        make_db_row(
            ws_id,
            db_id,
            "row-3",
            "▪",
            "SQLite WAL + FTS5 Sync Daemon",
            ("Done", NotionColor::Green),
            ("P0 High", NotionColor::Red),
            &[("Core", NotionColor::Purple), ("API", NotionColor::Green)],
            "Liam Brooks",
            "2025-11-19T12:15:00Z",
        ),
        make_db_row(
            ws_id,
            db_id,
            "row-4",
            "▪",
            "Stdio MCP Server (18 Tools)",
            ("Done", NotionColor::Green),
            ("P1 Med", NotionColor::Orange),
            &[("MCP", NotionColor::Pink), ("API", NotionColor::Green)],
            "Priya Nair",
            "2025-11-19T11:30:00Z",
        ),
        make_db_row(
            ws_id,
            db_id,
            "row-5",
            "▪",
            "OAuth 2.0 Multi-Workspace Auth",
            ("In Progress", NotionColor::Blue),
            ("P1 Med", NotionColor::Orange),
            &[("Core", NotionColor::Purple)],
            "Alex Rivera",
            "2025-11-19T10:45:00Z",
        ),
        make_db_row(
            ws_id,
            db_id,
            "row-6",
            "▪",
            "Kitty & Sixel Inline HD Images",
            ("In Review", NotionColor::Purple),
            ("P1 Med", NotionColor::Orange),
            &[("TUI", NotionColor::Blue)],
            "Maya Chen",
            "2025-11-18T18:10:00Z",
        ),
        make_db_row(
            ws_id,
            db_id,
            "row-7",
            "▪",
            "Offline Markdown Export Bundle",
            ("Not Started", NotionColor::Gray),
            ("P2 Low", NotionColor::Yellow),
            &[("API", NotionColor::Green)],
            "Liam Brooks",
            "2025-11-18T15:00:00Z",
        ),
        make_db_row(
            ws_id,
            db_id,
            "row-8",
            "▪",
            "Webhook Event Stream Bridge",
            ("Not Started", NotionColor::Gray),
            ("P2 Low", NotionColor::Yellow),
            &[("MCP", NotionColor::Pink)],
            "Priya Nair",
            "2025-11-17T14:20:00Z",
        ),
    ];

    let pages = vec![page_specs, page_launch.clone(), page_arch, page_design, page_notes];
    let databases = vec![db_roadmap, db_feedback];

    // Seed in-memory SQLite FTS5 so Quick Find (Ctrl+K) returns real search results
    let _ = app.storage.upsert_pages(&pages);
    let _ = app.storage.upsert_pages(&db_rows);
    let _ = app.storage.upsert_databases(&databases);

    app.pages = pages.iter().map(Into::into).collect();
    app.databases = databases;
    app.active_view = CanvasView::Page;
    app.active_page_id = Some(page_launch.id.clone());
    app.active_page_detail = Some(page_detail);
    app.active_database_id = Some(db_id.into());
    app.active_database_rows = db_rows;
    app.nav_stack = vec![NavTarget::Page("page-specs".into())];

    (app, ui)
}

#[tokio::main]
async fn main() {
    let (mut app, mut ui) = build_demo_state();
    let mut frames = Vec::new();

    // Frame 1: Centered Editorial Page Canvas
    app.active_view = CanvasView::Page;
    ui.show_sidebar = true;
    ui.page_width = PageWidthMode::Centered;
    ui.comments_drawer_open = false;
    ui.focus = Focus::Canvas;
    ui.selected_block = 3; // Highlight unchecked to-do
    ui.expanded_toggles.clear();
    ui.overlays.clear();
    frames.push(capture_frame(
        &mut app,
        &mut ui,
        2400,
        "Editorial Page Canvas — Centered measure, Notion property pills & rich blocks",
        "Page",
    ));

    // Frame 2: Toggle expanded + To-do checked (Space / x)
    ui.expanded_toggles.insert("blk-toggle-1".into());
    if let Some(detail) = app.active_page_detail.as_mut() {
        for blk in &mut detail.blocks {
            if blk.id == "blk-todo-2" {
                if let NotionBlockKind::ToDo {
                    checked: ref mut c, ..
                } = blk.kind
                {
                    *c = true;
                }
            }
        }
    }
    ui.selected_block = 5; // Highlight expanded toggle
    frames.push(capture_frame(
        &mut app,
        &mut ui,
        2400,
        "Interactive Blocks — Live to-do check & collapsible toggle expansion",
        "Space / x",
    ));

    // Frame 3: Toggle Full Width mode (w) with sidebar collapsed for spacious full-width reading
    ui.show_sidebar = false;
    ui.page_width = PageWidthMode::Full;
    ui.selected_block = 8; // Highlight code block
    frames.push(capture_frame(
        &mut app,
        &mut ui,
        2200,
        "Full Width Canvas — Distraction-free wide mode (`w` width · `[` sidebar)",
        "w",
    ));

    // Frame 4: Database Table View (1)
    app.active_view = CanvasView::Database;
    app.nav_stack.clear();
    ui.show_sidebar = true;
    ui.focus = Focus::Canvas;
    ui.db_view_mode = DatabaseViewMode::Table;
    ui.db_sort = DatabaseSortMode::LastEditedDesc;
    ui.db_selected_row = 0;
    frames.push(capture_frame(
        &mut app,
        &mut ui,
        2400,
        "Database Table View — Canonical Status, Select & Multi-Select pills",
        "1 Table",
    ));

    // Frame 5: Database Board (Kanban) View (2) grouped by Status
    ui.show_sidebar = false; // Full 120-col width renders all 4 Kanban columns cleanly
    ui.db_view_mode = DatabaseViewMode::Board;
    ui.db_board_col = 1;
    ui.db_board_row = 0;
    frames.push(capture_frame(
        &mut app,
        &mut ui,
        2400,
        "Database Board View — Multi-column Kanban grouped by Status (m to move)",
        "2 Board",
    ));

    // Frame 6: Database List View (3)
    ui.show_sidebar = true;
    ui.db_view_mode = DatabaseViewMode::List;
    ui.db_selected_row = 1;
    frames.push(capture_frame(
        &mut app,
        &mut ui,
        2000,
        "Database List View — Clean compact view with live filter & sort",
        "3 List",
    ));

    // Frame 7: Comments Peek Drawer (C) alongside Editorial Page Canvas
    app.active_view = CanvasView::Page;
    app.nav_stack = vec![NavTarget::Page("page-specs".into())];
    ui.page_width = PageWidthMode::Full;
    ui.comments_drawer_open = true;
    ui.focus = Focus::Comments;
    ui.comment_reply_input = "LGTM — ready to tag v1.0!".into();
    frames.push(capture_frame(
        &mut app,
        &mut ui,
        2200,
        "Comments Peek Drawer — Read threads & reply inline without leaving the page",
        "C",
    ));

    // Frame 8: Quick Find / Command Palette (Ctrl+K)
    ui.comments_drawer_open = false;
    ui.page_width = PageWidthMode::Centered;
    ui.focus = Focus::Canvas;
    let mut palette = CommandPalette::new();
    palette.selected = 1;
    ui.overlays = vec![Overlay::CommandPalette(palette)];
    frames.push(capture_frame(
        &mut app,
        &mut ui,
        2200,
        "Quick Find & Command Palette — Instant FTS5 search across pages, DBs & actions",
        "Ctrl+K",
    ));

    let spec = json!({
        "title": "notion — Acme Product HQ",
        "badge": "@bitefoundry/notion v0.1.0",
        "width": WIDTH,
        "height": HEIGHT,
        "bg": [25, 25, 25],
        "fg": [235, 235, 232],
        "frames": frames,
    });

    let temp_json = std::env::temp_dir().join("notion_demo_frames.json");
    std::fs::write(
        &temp_json,
        serde_json::to_string(&spec).expect("serialize frames.json"),
    )
    .expect("write temp frames.json");

    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let output_gif = repo_root.join("assets").join("demo.gif");
    if let Some(parent) = output_gif.parent() {
        std::fs::create_dir_all(parent).expect("create assets dir");
    }

    let script = renderer_script();
    if !script.exists() {
        eprintln!(
            "Renderer script not found at {}. Set DEMO_RENDERER_SCRIPT to render_tui_gif.py.\nFrames were kept at {}.",
            script.display(),
            temp_json.display()
        );
        std::process::exit(1);
    }
    let status = Command::new("python")
        .arg(&script)
        .arg(&temp_json)
        .arg(&output_gif)
        .arg("--preview-png")
        .arg(preview_dir())
        .status()
        .expect("invoke render_tui_gif.py");

    assert!(
        status.success(),
        "render_tui_gif.py failed with status {status}"
    );
    println!(
        "Rendered {} ({} frames)",
        output_gif.display(),
        spec["frames"].as_array().map(|a| a.len()).unwrap_or(0)
    );
}
