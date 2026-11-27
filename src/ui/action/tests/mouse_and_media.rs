use ratatui::{backend::TestBackend, layout::Rect, Terminal};

use super::make_test_app_and_ui;
use crate::{
    auth::AuthManager,
    config::AppConfig,
    img_preview::TerminalImageProtocol,
    models::{
        DatabaseViewMode, NotionBlock, NotionBlockKind, NotionMediaRef, NotionPage,
        NotionPageDetail, RichTextSpan,
    },
    notion::NotionClient,
    storage::Storage,
    sync::{FocusTarget, SyncController},
    ui::{
        self,
        action::{dispatch, Action},
        app::{AppState, CanvasView, Focus, UiMsg, UiState},
        hitmap::{ButtonId, HitTarget},
        overlays::{image_preview::ImagePreviewOverlay, Overlay, OverlayOutcome},
    },
};

#[test]
fn test_mouse_scroll_routes_by_hovered_pane_sidebar_canvas_database_and_comments() {
    let (mut app, mut ui) = make_test_app_and_ui();
    ui.focus = Focus::Canvas;
    ui.comments_drawer_open = true;

    let mut terminal = Terminal::new(TestBackend::new(140, 36)).unwrap();
    terminal
        .draw(|f| {
            ui::draw(f, &mut app, &mut ui);
        })
        .unwrap();

    // 1. Scroll over Sidebar (col = 5)
    let before_sidebar = ui.sidebar_selected;
    dispatch(
        Action::MouseScroll {
            delta: 1,
            column: 5,
            row: 10,
        },
        &mut app,
        &mut ui,
    );
    assert!(ui.sidebar_selected > before_sidebar);
    assert_eq!(ui.canvas_scroll, 0);
    assert_eq!(ui.comments_scroll, 0);

    // 2. Scroll over Page Canvas (col = 60)
    dispatch(
        Action::MouseScroll {
            delta: 4,
            column: 60,
            row: 10,
        },
        &mut app,
        &mut ui,
    );
    assert_eq!(ui.canvas_scroll, 4);
    assert_eq!(ui.comments_scroll, 0);

    // 3. Scroll over Comments Drawer (col = 130)
    dispatch(
        Action::MouseScroll {
            delta: 3,
            column: 130,
            row: 10,
        },
        &mut app,
        &mut ui,
    );
    assert_eq!(ui.comments_scroll, 3);
    assert_eq!(ui.canvas_scroll, 4);

    // 4. Switch to Database View (Table, List, Board)
    app.active_view = CanvasView::Database;
    app.active_database_id = Some("db-1".into());
    ui.comments_drawer_open = false;

    for mode in [DatabaseViewMode::Table, DatabaseViewMode::List] {
        ui.db_view_mode = mode;
        ui.db_selected_row = 0;
        terminal
            .draw(|f| {
                ui::draw(f, &mut app, &mut ui);
            })
            .unwrap();
        dispatch(
            Action::MouseScroll {
                delta: 2,
                column: 60,
                row: 10,
            },
            &mut app,
            &mut ui,
        );
        assert_eq!(ui.db_selected_row, 2, "failed for {mode:?}");
        dispatch(
            Action::MouseScroll {
                delta: -1,
                column: 60,
                row: 10,
            },
            &mut app,
            &mut ui,
        );
        assert_eq!(ui.db_selected_row, 1, "failed scroll up for {mode:?}");
    }

    // Board mode: hovering card in column 1 switches db_board_col and scrolls db_board_row
    ui.db_view_mode = DatabaseViewMode::Board;
    ui.db_board_col = 0;
    ui.db_board_row = 0;
    terminal
        .draw(|f| {
            ui::draw(f, &mut app, &mut ui);
        })
        .unwrap();
    let card_rect = ui
        .hitmap
        .region_of(HitTarget::DbBoardCard { col: 1, row: 0 })
        .expect("board card col=1 row=0 should be in hitmap");
    dispatch(
        Action::MouseScroll {
            delta: 1,
            column: card_rect.x + 1,
            row: card_rect.y + 1,
        },
        &mut app,
        &mut ui,
    );
    assert_eq!(ui.db_board_col, 1);
    assert_eq!(ui.db_board_row, 1);
}

#[test]
fn test_canvas_scroll_clamps_after_scroll_bottom_so_wheel_up_responds_immediately() {
    let (mut app, mut ui) = make_test_app_and_ui();
    ui.focus = Focus::Canvas;
    let mut terminal = Terminal::new(TestBackend::new(140, 36)).unwrap();

    dispatch(Action::ScrollBottom, &mut app, &mut ui);
    assert_eq!(ui.canvas_scroll, u16::MAX / 2);

    // Drawing clamps ui.canvas_scroll back to exact max_scroll
    terminal
        .draw(|f| {
            ui::draw(f, &mut app, &mut ui);
        })
        .unwrap();
    let clamped_max = ui.canvas_scroll;
    assert!(clamped_max > 0 && clamped_max < 200);

    // Immediately wheeling up moves canvas_scroll back by 3 without lag
    dispatch(
        Action::MouseScroll {
            delta: -3,
            column: 60,
            row: 10,
        },
        &mut app,
        &mut ui,
    );
    assert_eq!(ui.canvas_scroll, clamped_max.saturating_sub(3));
}

#[test]
fn test_clicking_outside_overlay_on_canvas_margin_closes_overlay_and_clears_pixel_graphic() {
    let (mut app, mut ui) = make_test_app_and_ui();
    ui.overlays.push(Overlay::help());
    ui.active_pixel_graphic = Some((
        "hd:page-root:0".into(),
        Rect::new(10, 5, 40, 20),
        TerminalImageProtocol::Sixel,
    ));

    let mut terminal = Terminal::new(TestBackend::new(140, 36)).unwrap();
    terminal
        .draw(|f| {
            ui::draw(f, &mut app, &mut ui);
        })
        .unwrap();

    // Top-left margin inside sidebar/canvas outside the centered Help overlay
    let target = ui
        .hitmap
        .hit(2, 2)
        .expect("outside click must hit a Pane backdrop target");
    assert!(!matches!(
        target,
        HitTarget::OverlayBody | HitTarget::OverlayItem { .. }
    ));

    dispatch(
        Action::MouseClick {
            target,
            col: 2,
            row: 2,
        },
        &mut app,
        &mut ui,
    );
    assert!(ui.overlays.is_empty());
    assert!(ui.active_pixel_graphic.is_none());
}

#[tokio::test]
async fn test_clicking_nth_image_chip_opens_matching_media_index_and_sidebar_arrow_toggles_tree()
{
    let (mut app, mut ui) = make_test_app_and_ui();
    let mut terminal = Terminal::new(TestBackend::new(140, 36)).unwrap();
    terminal
        .draw(|f| {
            ui::draw(f, &mut app, &mut ui);
        })
        .unwrap();

    // 1. Verify Bookmark block registered as ChildCard(2)
    assert!(ui.hitmap.region_of(HitTarget::ChildCard(2)).is_some());

    // 2. Clicking 2nd MediaChip (block index 1) opens ImagePreviewOverlay at index == 1
    let chip2 = ui
        .hitmap
        .region_of(HitTarget::MediaChip(1))
        .expect("2nd image MediaChip should be mapped");
    let hit = ui.hitmap.hit(chip2.y, chip2.x).unwrap();
    assert_eq!(hit, HitTarget::MediaChip(1));
    dispatch(
        Action::MouseClick {
            target: hit,
            col: chip2.x,
            row: chip2.y,
        },
        &mut app,
        &mut ui,
    );
    match ui.overlays.last() {
        Some(Overlay::ImagePreview(ov)) => {
            assert_eq!(ov.index, 1);
            assert_eq!(ov.current().unwrap().url, "https://example.com/2.png");
        }
        other => panic!("expected ImagePreview overlay, got {other:?}"),
    }
    ui.overlays.clear();

    // 3. Clicking SidebarToggle arrow toggles collapsed_nodes while keeping Focus::Sidebar
    let items = app.build_sidebar_items(&ui);
    let root_idx = items
        .iter()
        .position(|it| it.id == "page-root")
        .expect("root page in sidebar");
    let toggle_rect = ui
        .hitmap
        .region_of(HitTarget::SidebarToggle(root_idx))
        .expect("collapsible root page arrow should be in hitmap");
    let arrow_hit = ui.hitmap.hit(toggle_rect.y, toggle_rect.x + 1).unwrap();
    assert_eq!(arrow_hit, HitTarget::SidebarToggle(root_idx));

    ui.focus = Focus::Canvas;
    dispatch(
        Action::MouseClick {
            target: arrow_hit,
            col: toggle_rect.x + 1,
            row: toggle_rect.y,
        },
        &mut app,
        &mut ui,
    );
    assert_eq!(ui.focus, Focus::Sidebar);
    assert!(ui.collapsed_nodes.contains("page-root"));

    // Clicking again expands it and stays in Focus::Sidebar
    dispatch(
        Action::MouseClick {
            target: arrow_hit,
            col: toggle_rect.x + 1,
            row: toggle_rect.y,
        },
        &mut app,
        &mut ui,
    );
    assert_eq!(ui.focus, Focus::Sidebar);
    assert!(!ui.collapsed_nodes.contains("page-root"));

    // 4. Page Header metadata badges (💬 N and 🖼 N media) are clickable
    let comment_btn = ui
        .hitmap
        .region_of(HitTarget::Button(ButtonId::ToggleComments))
        .expect("comment metadata badge should be clickable");
    let media_btn = ui
        .hitmap
        .region_of(HitTarget::Button(ButtonId::ImagePreview))
        .expect("media metadata badge should be clickable");
    assert!(media_btn.x > comment_btn.x);
}

#[tokio::test]
async fn test_html_returned_from_image_auto_routes_to_html_preview() {
    use axum::{http::header, routing::get, Router};
    use tokio::net::TcpListener;

    // Mock HTTP endpoint pretending to be an image URL but returning HTML!
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let html_body = "<!DOCTYPE html><html><head><title>Auth Gate</title></head><body><h1>Sign in required</h1><button onclick=\"document.querySelector('h1').textContent='Clicked'\">Continue</button></body></html>";
    let router = Router::new().route(
        "/signed-photo",
        get(move || async move {
            (
                axum::http::StatusCode::OK,
                [(header::CONTENT_TYPE, "image/png")],
                html_body,
            )
        }),
    );
    let server_handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let cfg = AppConfig::default();
    let auth = AuthManager::from_token(cfg.clone(), "ntn_route_test");
    let client = NotionClient::new(&cfg, auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let (focus_tx, _focus_rx) = tokio::sync::watch::channel(FocusTarget::default());
    let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);

    let mut ui = UiState::from_config(&cfg);
    let mut app = AppState::new(cfg, auth, client, storage, sync_ctrl, ui_tx);
    let page = NotionPage {
        id: "page-html-img".into(),
        title: "HTML Image Test".into(),
        ..Default::default()
    };
    let url = format!("http://{addr}/signed-photo");
    app.active_view = CanvasView::Page;
    app.active_page_id = Some(page.id.clone());
    app.active_page_detail = Some(NotionPageDetail {
        page,
        blocks: vec![NotionBlock {
            id: "blk-img-html".into(),
            page_id: "page-html-img".into(),
            kind: NotionBlockKind::Image {
                url: url.clone(),
                caption: vec![RichTextSpan::plain("Broken Photo")],
                is_hosted: false,
            },
            ..Default::default()
        }],
        comments: Vec::new(),
        comments_error: None,
        markdown: String::new(),
        media: vec![NotionMediaRef {
            block_id: "blk-img-html".into(),
            url,
            display_name: "Broken Photo".into(),
            mime_type: "image/png".into(),
            is_image: true,
        }],
    });

    dispatch(Action::OpenImagePreview, &mut app, &mut ui);
    assert!(matches!(ui.overlays.last(), Some(Overlay::ImagePreview(_))));

    // Wait for LoadPreviewImage background task to sniff HTML and send HtmlPreviewLoaded
    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ui_rx.recv())
        .await
        .expect("timed out waiting for HtmlPreviewLoaded")
        .expect("ui_rx closed");
    match &msg {
        UiMsg::HtmlPreviewLoaded { title, html, .. } => {
            assert_eq!(title, "Broken Photo");
            assert!(html.contains("Sign in required"));
        }
        _ => panic!("expected UiMsg::HtmlPreviewLoaded, got another UiMsg"),
    }

    app.handle_ui_msg(&mut ui, msg);
    assert_eq!(ui.overlays.len(), 1);
    match ui.overlays.last() {
        Some(Overlay::HtmlPreview(ov)) => {
            assert!(ov.session.js_active);
            assert!(ov.session.raw_html().contains("Sign in required"));
        }
        other => panic!("expected Overlay::HtmlPreview replacing ImagePreview, got {other:?}"),
    }

    server_handle.abort();
}

#[test]
fn test_html_file_block_mime_code_preview_pill_notion_button_and_all_button_pills() {
    use crate::parser::{collect_media_refs, parse_notion_block};

    // 1. Parse Notion `button` blocks (rich_text, plain text, and fallback "Button")
    let btn_rt = parse_notion_block(
        &serde_json::json!({
            "id": "blk-btn-1",
            "type": "button",
            "button": { "rich_text": [{ "plain_text": "Deploy Release" }] }
        }),
        "page-1",
        None,
    );
    assert_eq!(btn_rt.plain_text(), "Deploy Release");
    assert!(matches!(btn_rt.kind, NotionBlockKind::Button { .. }));

    let btn_fallback = parse_notion_block(
        &serde_json::json!({
            "id": "blk-btn-2",
            "type": "button",
            "button": {}
        }),
        "page-1",
        None,
    );
    assert_eq!(btn_fallback.plain_text(), "Button");

    // 2. Parse `.html` File block & `.html`/`.svg` Image blocks -> verify media refs never mark them as raster image!
    let html_file = parse_notion_block(
        &serde_json::json!({
            "id": "blk-file-html",
            "type": "file",
            "file": {
                "type": "external",
                "name": "dashboard.html",
                "external": { "url": "https://example.com/dashboard.html" }
            }
        }),
        "page-1",
        None,
    );
    let fake_img_html = parse_notion_block(
        &serde_json::json!({
            "id": "blk-img-htm",
            "type": "image",
            "image": {
                "type": "external",
                "external": { "url": "https://example.com/viewer.htm" }
            }
        }),
        "page-1",
        None,
    );
    let fake_img_svg = parse_notion_block(
        &serde_json::json!({
            "id": "blk-img-svg",
            "type": "image",
            "image": {
                "type": "external",
                "external": { "url": "https://example.com/diagram.svg" }
            }
        }),
        "page-1",
        None,
    );
    let html_code = NotionBlock {
        id: "blk-code-html".into(),
        page_id: "page-1".into(),
        kind: NotionBlockKind::Code {
            rich_text: vec![RichTextSpan::plain(
                "<h1>Embedded HTML</h1><button onclick=\"alert('hi')\">Go</button>",
            )],
            language: "html".into(),
            caption: Vec::new(),
        },
        ..Default::default()
    };

    let blocks = vec![btn_rt, html_file, fake_img_html, fake_img_svg, html_code];
    let media_refs = collect_media_refs(&blocks);
    assert_eq!(media_refs.len(), 3);
    assert_eq!(media_refs[0].mime_type, "text/html");
    assert!(!media_refs[0].is_image);
    assert_eq!(media_refs[1].mime_type, "text/html");
    assert!(!media_refs[1].is_image);
    assert_eq!(media_refs[2].mime_type, "image/svg+xml");
    assert!(media_refs[2].is_image);

    // 3. Render page canvas with these blocks and verify HitMap targets + ButtonIds
    let (mut app, mut ui) = make_test_app_and_ui();
    let page = NotionPage {
        id: "page-1".into(),
        title: "Interactive Page".into(),
        url: "https://www.notion.so/page-1".into(),
        ..Default::default()
    };
    app.active_view = CanvasView::Page;
    app.active_page_id = Some(page.id.clone());
    app.active_page_detail = Some(NotionPageDetail {
        page,
        blocks,
        comments: Vec::new(),
        comments_error: None,
        markdown: String::new(),
        media: media_refs,
    });
    ui.focus = Focus::Canvas;

    let mut terminal = Terminal::new(TestBackend::new(140, 36)).unwrap();
    terminal
        .draw(|f| {
            ui::draw(f, &mut app, &mut ui);
        })
        .unwrap();
    let screen: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(screen.contains("⚡") && screen.contains("Deploy Release"));
    assert!(screen.contains("Preview HTML"));
    assert!(screen.contains("dashboard.html"));

    // Verify previously missing ButtonIds are now wired in HitMap!
    for btn in [
        ButtonId::ToggleSidebar,
        ButtonId::Search,
        ButtonId::CommandPalette,
        ButtonId::ToggleFavorite,
        ButtonId::QuickAppend,
        ButtonId::NewChildPage,
        ButtonId::EditPage,
        ButtonId::OpenWeb,
        ButtonId::LinkExtractor,
        ButtonId::ToggleHrefs,
        ButtonId::Attachments,
    ] {
        assert!(
            ui.hitmap.region_of(HitTarget::Button(btn)).is_some(),
            "expected ButtonId::{btn:?} to be registered in HitMap"
        );
    }

    // 4. Clicking Notion `button` block shows toast & Confirm dialog offering to open Notion page URL
    let btn_rect = ui
        .hitmap
        .region_of(HitTarget::NotionButton(0))
        .expect("NotionButton(0) should be in hitmap");
    dispatch(
        Action::MouseClick {
            target: HitTarget::NotionButton(0),
            col: btn_rect.x + 2,
            row: btn_rect.y,
        },
        &mut app,
        &mut ui,
    );
    assert!(
        ui.toasts
            .latest()
            .is_some_and(|t| t.message.contains("Deploy Release")),
        "clicking Notion button should display an informative toast"
    );
    assert!(matches!(ui.overlays.last(), Some(Overlay::Confirm(_))));
    ui.overlays.clear();

    // 5. Clicking `[▶ Preview HTML]` pill on the HTML code block opens Overlay::HtmlPreview!
    let code_pill = ui
        .hitmap
        .region_of(HitTarget::CodeHtmlPreview(4))
        .expect("CodeHtmlPreview(4) pill should be in hitmap");
    dispatch(
        Action::MouseClick {
            target: HitTarget::CodeHtmlPreview(4),
            col: code_pill.x + 1,
            row: code_pill.y,
        },
        &mut app,
        &mut ui,
    );
    assert!(matches!(ui.overlays.last(), Some(Overlay::HtmlPreview(_))));
    ui.overlays.clear();

    // 6. Pressing 'v' in Attachments overlay on `.html` item dispatches FetchHtmlPreview,
    //    and on `.svg` item opens native ImagePreviewOverlay!
    let mut att = crate::ui::overlays::attachments::AttachmentsState::default();
    match att.handle(&Action::Input('v'), &app) {
        OverlayOutcome::CloseAndDispatch(Action::FetchHtmlPreview { title, url, .. }) => {
            assert_eq!(title, "dashboard.html");
            assert_eq!(url, "https://example.com/dashboard.html");
        }
        other => panic!("expected FetchHtmlPreview for HTML attachment, got {other:?}"),
    }
    att.selected = 2; // diagram.svg
    match att.handle(&Action::Input('v'), &app) {
        OverlayOutcome::CloseAndDispatch(Action::OpenImagePreviewAt(2)) => {}
        other => panic!("expected OpenImagePreviewAt(2) for SVG attachment, got {other:?}"),
    }
}

#[tokio::test]
async fn test_open_media_url_refreshes_signed_s3_url_and_svg_renders_natively_in_image_preview() {
    use axum::{
        extract::Path as AxumPath,
        http::header,
        routing::get,
        Json, Router,
    };
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let fresh_s3_url = "https://prod-files-secure.s3.us-west-2.amazonaws.com/ws/blk-s3/fresh.png?X-Amz-Expires=3600&X-Amz-Signature=fresh_sig_999".to_string();
    let fresh_for_route = fresh_s3_url.clone();
    let svg_body = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"24\" height=\"24\"><rect width=\"24\" height=\"24\" fill=\"teal\"/></svg>";

    let router = Router::new()
        .route(
            "/v1/blocks/{id}",
            get(move |AxumPath(id): AxumPath<String>| {
                let fresh = fresh_for_route.clone();
                async move {
                    Json(serde_json::json!({
                        "id": id,
                        "type": "image",
                        "has_children": false,
                        "image": {
                            "type": "file",
                            "file": { "url": fresh },
                            "caption": [{ "plain_text": "Signed Shot" }]
                        }
                    }))
                }
            }),
        )
        .route(
            "/diagram.svg",
            get(move || async move {
                (
                    axum::http::StatusCode::OK,
                    [(header::CONTENT_TYPE, "image/svg+xml")],
                    svg_body,
                )
            }),
        );
    let server_handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let cfg = AppConfig::default();
    let auth = AuthManager::from_token(cfg.clone(), "ntn_s3_refresh_test");
    let client = NotionClient::new(&cfg, auth.clone())
        .unwrap()
        .with_base_url(format!("{base_url}/v1"));
    let storage = Storage::open_in_memory().unwrap();
    let (focus_tx, _focus_rx) = tokio::sync::watch::channel(FocusTarget::default());
    let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);

    let mut ui = UiState::from_config(&cfg);
    let mut app = AppState::new(cfg, auth, client, storage, sync_ctrl, ui_tx);

    let stale_s3_url = "https://prod-files-secure.s3.us-west-2.amazonaws.com/ws/blk-s3/old.png?X-Amz-Expires=3600&X-Amz-Signature=expired_sig_000".to_string();
    let svg_url = format!("{base_url}/diagram.svg");
    let page = NotionPage {
        id: "page-s3".into(),
        title: "S3 Refresh Test".into(),
        ..Default::default()
    };
    app.active_view = CanvasView::Page;
    app.active_page_id = Some(page.id.clone());
    app.active_page_detail = Some(NotionPageDetail {
        page: page.clone(),
        blocks: vec![NotionBlock {
            id: "blk-s3".into(),
            page_id: page.id.clone(),
            kind: NotionBlockKind::Image {
                url: stale_s3_url.clone(),
                caption: vec![RichTextSpan::plain("Signed Shot")],
                is_hosted: true,
            },
            ..Default::default()
        }],
        comments: Vec::new(),
        comments_error: None,
        markdown: String::new(),
        media: vec![
            NotionMediaRef {
                block_id: "blk-s3".into(),
                url: stale_s3_url.clone(),
                display_name: "Signed Shot".into(),
                mime_type: "image/png".into(),
                is_image: true,
            },
            NotionMediaRef {
                block_id: "blk-svg".into(),
                url: svg_url.clone(),
                display_name: "diagram.svg".into(),
                mime_type: "image/png".into(),
                is_image: true,
            },
        ],
    });

    // Open ImagePreviewOverlay and press 'o' -> dispatches OpenMediaUrl and refreshes signed S3 URL!
    let ov = ImagePreviewOverlay::from_notion_page(app.active_page_detail.as_ref().unwrap(), Some(0)).unwrap();
    assert_eq!(ov.current().unwrap().block_id, "blk-s3");
    ui.overlays.push(Overlay::ImagePreview(ov));

    dispatch(Action::Input('o'), &mut app, &mut ui);

    let mut got_refreshed = false;
    for _ in 0..3 {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ui_rx.recv())
            .await
            .expect("timed out waiting for MediaUrlRefreshed")
            .expect("ui_rx closed");
        if let UiMsg::MediaUrlRefreshed {
            ref page_id,
            ref block_id,
            ref url,
        } = msg
        {
            assert_eq!(page_id, "page-s3");
            assert_eq!(block_id, "blk-s3");
            assert_eq!(url, &fresh_s3_url);
            got_refreshed = true;
        }
        app.handle_ui_msg(&mut ui, msg);
        if got_refreshed {
            break;
        }
    }
    assert!(got_refreshed, "expected UiMsg::MediaUrlRefreshed");
    assert_eq!(
        app.active_page_detail.as_ref().unwrap().media[0].url,
        fresh_s3_url
    );
    match ui.overlays.last() {
        Some(Overlay::ImagePreview(ov)) => {
            assert_eq!(ov.items[0].url, fresh_s3_url);
        }
        other => panic!("expected ImagePreviewOverlay with refreshed URL, got {other:?}"),
    }

    // Now verify LoadPreviewImage on genuine SVG renders natively via ImagePreviewOverlay (no <div> HTML wrapping!)
    // or returns a clean feature-gated error when compiled without `svg-graphics`.
    if let Some(Overlay::ImagePreview(ref mut ov)) = ui.overlays.last_mut() {
        ov.index = 1; // diagram.svg
    }
    dispatch(Action::LoadPreviewImage, &mut app, &mut ui);
    #[cfg(feature = "svg-graphics")]
    {
        let mut got_svg_rendered = false;
        for _ in 0..4 {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ui_rx.recv())
                .await
                .expect("timed out waiting for SVG ImageRendered")
                .expect("ui_rx closed");
            if let UiMsg::ImageRendered { ref rendered, .. } = msg {
                assert_eq!(rendered.format_label, "SVG");
                assert_eq!(rendered.width_px, 24);
                assert_eq!(rendered.height_px, 24);
                assert!(!rendered.lines.is_empty() || rendered.pixel_payload.is_some());
                got_svg_rendered = true;
                app.handle_ui_msg(&mut ui, msg);
                break;
            }
            app.handle_ui_msg(&mut ui, msg);
        }
        assert!(
            got_svg_rendered,
            "expected SVG to render natively via ImageRendered"
        );
    }
    #[cfg(not(feature = "svg-graphics"))]
    {
        let mut got_svg_err = false;
        for _ in 0..4 {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ui_rx.recv())
                .await
                .expect("timed out waiting for SVG ImagePreviewFailed")
                .expect("ui_rx closed");
            if let UiMsg::ImagePreviewFailed { ref error, .. } = msg {
                assert!(error.contains("svg-graphics"), "unexpected error: {error}");
                got_svg_err = true;
                app.handle_ui_msg(&mut ui, msg);
                break;
            }
            app.handle_ui_msg(&mut ui, msg);
        }
        assert!(
            got_svg_err,
            "expected SVG preview to return a clean svg-graphics feature error"
        );
    }
    assert!(matches!(ui.overlays.last(), Some(Overlay::ImagePreview(_))));

    server_handle.abort();
}

#[test]
fn test_canvas_move_down_advances_scroll_and_keeps_selected_block_visible() {
    let (mut app, mut ui) = make_test_app_and_ui();
    ui.focus = Focus::Canvas;
    let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();

    terminal
        .draw(|f| {
            ui::draw(f, &mut app, &mut ui);
        })
        .unwrap();
    assert_eq!(ui.selected_block, 0);
    assert_eq!(ui.canvas_scroll, 0);

    // MoveDown increments selected_block AND advances canvas_scroll
    dispatch(Action::MoveDown, &mut app, &mut ui);
    assert_eq!(ui.selected_block, 1);
    assert_eq!(ui.canvas_scroll, 1);
    assert!(ui.canvas_follow_selected);

    // Navigating 35 blocks down auto-scrolls canvas_scroll so selected block stays in viewport
    for _ in 0..35 {
        dispatch(Action::MoveDown, &mut app, &mut ui);
    }
    assert_eq!(ui.selected_block, 36);
    terminal
        .draw(|f| {
            ui::draw(f, &mut app, &mut ui);
        })
        .unwrap();
    assert!(ui.canvas_scroll >= 15, "expected canvas_scroll to follow selected block, got {}", ui.canvas_scroll);
    assert!(!ui.canvas_follow_selected);

    // MouseScroll freely scrolls without snapping back to selected_block
    let before_wheel = ui.canvas_scroll;
    dispatch(
        Action::MouseScroll {
            delta: -6,
            column: 60,
            row: 10,
        },
        &mut app,
        &mut ui,
    );
    terminal
        .draw(|f| {
            ui::draw(f, &mut app, &mut ui);
        })
        .unwrap();
    assert_eq!(ui.canvas_scroll, before_wheel.saturating_sub(6));
}
