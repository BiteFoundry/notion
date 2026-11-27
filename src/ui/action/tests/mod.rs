mod block_live;
mod inline_editor;
mod mouse_and_media;
mod regressions;

use std::collections::BTreeMap;

use crate::{
    auth::AuthManager,
    config::AppConfig,
    models::{
        NotionBlock, NotionBlockKind, NotionColor, NotionComment, NotionDatabase, NotionMediaRef,
        NotionPage, NotionPageDetail, NotionPropertySchema, NotionPropertyValue, RichTextSpan,
        SelectOption,
    },
    notion::NotionClient,
    storage::Storage,
    sync::{FocusTarget, SyncController},
    ui::app::{AppState, CanvasView, UiState},
};

pub(super) fn make_test_app_and_ui() -> (AppState, UiState) {
    let cfg = AppConfig::default();
    let auth = AuthManager::from_token(cfg.clone(), "ntn_unit_test_token");
    let client = NotionClient::new(&cfg, auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let (focus_tx, _focus_rx) = tokio::sync::watch::channel(FocusTarget::default());
    let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let (ui_tx, _ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);

    let mut ui_state = UiState::from_config(&cfg);
    ui_state.show_sidebar = true;
    let mut app = AppState::new(cfg, auth, client, storage, sync_ctrl, ui_tx);

    let root_page = NotionPage {
        id: "page-root".into(),
        title: "Root Page".into(),
        parent_type: "workspace".into(),
        ..Default::default()
    };
    let child_page = NotionPage {
        id: "page-child".into(),
        title: "Child Page".into(),
        parent_type: "page_id".into(),
        parent_id: Some("page-root".into()),
        ..Default::default()
    };
    let sibling_page = NotionPage {
        id: "page-sibling".into(),
        title: "Sibling Page".into(),
        parent_type: "workspace".into(),
        ..Default::default()
    };
    app.pages = [&root_page, &child_page, &sibling_page].into_iter().map(Into::into).collect();

    let mut schema = BTreeMap::new();
    schema.insert(
        "Status".into(),
        NotionPropertySchema {
            id: "st".into(),
            name: "Status".into(),
            prop_type: "status".into(),
            options: vec![
                SelectOption {
                    id: Some("1".into()),
                    name: "Todo".into(),
                    color: NotionColor::Gray,
                },
                SelectOption {
                    id: Some("2".into()),
                    name: "Done".into(),
                    color: NotionColor::Green,
                },
            ],
            relation_database_id: None,
        },
    );
    app.databases = vec![NotionDatabase {
        id: "db-1".into(),
        title: "Tasks".into(),
        schema,
        ..Default::default()
    }];
    app.active_database_rows = (0..6)
        .map(|i| {
            let mut props = BTreeMap::new();
            let status = if i < 3 { "Todo" } else { "Done" };
            props.insert(
                "Status".into(),
                NotionPropertyValue::Status(Some(SelectOption {
                    id: None,
                    name: status.into(),
                    color: NotionColor::Blue,
                })),
            );
            NotionPage {
                id: format!("row-{i}"),
                title: format!("Task {i}"),
                parent_type: "database_id".into(),
                parent_id: Some("db-1".into()),
                properties: props,
                ..Default::default()
            }
        })
        .collect();

    let mut blocks: Vec<NotionBlock> = (0..50)
        .map(|i| NotionBlock {
            id: format!("blk-p-{i}"),
            page_id: "page-root".into(),
            kind: NotionBlockKind::Paragraph {
                rich_text: vec![RichTextSpan::plain(format!("Paragraph line number {i}"))],
                color: NotionColor::Default,
            },
            ..Default::default()
        })
        .collect();
    blocks.insert(
        0,
        NotionBlock {
            id: "blk-img-1".into(),
            page_id: "page-root".into(),
            kind: NotionBlockKind::Image {
                url: "https://example.com/1.png".into(),
                caption: vec![RichTextSpan::plain("First image")],
                is_hosted: false,
            },
            ..Default::default()
        },
    );
    blocks.insert(
        1,
        NotionBlock {
            id: "blk-img-2".into(),
            page_id: "page-root".into(),
            kind: NotionBlockKind::Image {
                url: "https://example.com/2.png".into(),
                caption: vec![RichTextSpan::plain("Second image")],
                is_hosted: false,
            },
            ..Default::default()
        },
    );
    blocks.insert(
        2,
        NotionBlock {
            id: "blk-bookmark".into(),
            page_id: "page-root".into(),
            kind: NotionBlockKind::Bookmark {
                url: "https://notion.so".into(),
                caption: vec![RichTextSpan::plain("Notion Docs")],
            },
            ..Default::default()
        },
    );

    let comments: Vec<NotionComment> = (0..15)
        .map(|i| NotionComment {
            id: format!("c-{i}"),
            page_id: "page-root".into(),
            author: format!("User {i}"),
            body: format!("Comment body line {i}"),
            created_time: "2026-11-20T01:00:00Z".into(),
            ..Default::default()
        })
        .collect();

    let media = vec![
        NotionMediaRef {
            block_id: "blk-img-1".into(),
            url: "https://example.com/1.png".into(),
            display_name: "First image".into(),
            mime_type: "image/png".into(),
            is_image: true,
        },
        NotionMediaRef {
            block_id: "blk-img-2".into(),
            url: "https://example.com/2.png".into(),
            display_name: "Second image".into(),
            mime_type: "image/png".into(),
            is_image: true,
        },
    ];

    app.active_view = CanvasView::Page;
    app.active_page_id = Some("page-root".into());
    app.active_page_detail = Some(NotionPageDetail {
        page: root_page,
        blocks,
        comments,
        comments_error: None,
        markdown: String::new(),
        media,
    });
    (app, ui_state)
}
