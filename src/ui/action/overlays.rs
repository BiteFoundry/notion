//! Overlay openers, HTML/image preview, media fetching, clipboard, and link actions.

use bitefoundry_html::{sniff_content_kind, ContentKind};

use crate::{
    config::ImageProtocolPreference,
    models::{spans_plain_text, NotionBlockKind},
    notion::media,
    parser::parse_slash_command_blocks,
    ui::{
        app::{AppState, CanvasView, Focus, UiMsg, UiState},
        hitmap::HitTarget,
        overlays::{
            attachments,
            context_menu::{ContextMenu, ContextTarget},
            image_preview::ImagePreviewOverlay,
            links::LinkExtractorOverlay,
            Overlay, OverlayKind,
        },
    },
};

use super::{
    copy_to_clipboard, dispatch, find_media_index_for_block, open_url_in_browser,
    selected_db_row_page, Action,
};

pub(super) fn try_dispatch(
    action: Action,
    app: &mut AppState,
    ui: &mut UiState,
) -> Result<bool, Action> {
    match action {
        Action::OpenOverlay(kind) => match kind {
            OverlayKind::CommandPalette => ui.overlays.push(Overlay::command_palette()),
            OverlayKind::WorkspaceSwitcher => ui.overlays.push(Overlay::workspace_switcher()),
            OverlayKind::Settings => ui.overlays.push(Overlay::settings()),
            OverlayKind::Help => ui.overlays.push(Overlay::help()),
            OverlayKind::Search => ui.overlays.push(Overlay::search()),
            OverlayKind::Attachments => {
                if app.active_view != CanvasView::Page {
                    ui.info("Open a Notion page to view media & files");
                    return Ok(false);
                }
                let mut tray = attachments::AttachmentsState::default();
                if let Some(std::borrow::Cow::Owned(draft)) = app.detail_for_block_action(ui) {
                    tray.draft_media = Some(draft.media);
                }
                ui.overlays.push(Overlay::Attachments(tray));
            }
            OverlayKind::FilePicker => ui.overlays.push(Overlay::file_picker()),
            OverlayKind::PathInput => ui.overlays.push(Overlay::path_input()),
            _ => {}
        },
        Action::OpenSearch => {
            ui.overlays.push(Overlay::search());
        }
        Action::OpenFilePicker => {
            ui.overlays.push(Overlay::file_picker());
        }
        Action::OpenContextMenu { col, row } => {
            if !ui.overlays.is_empty() {
                return Ok(false);
            }
            if ui.focus == Focus::Sidebar {
                app.snap_sidebar_selection(ui);
            }
            let target = match ui.focus {
                Focus::Sidebar => ContextTarget::Sidebar,
                Focus::Canvas if app.active_view == CanvasView::Database => ContextTarget::Database,
                Focus::Canvas => ContextTarget::PageCanvas,
                _ => ContextTarget::Global,
            };
            let menu = ContextMenu::build(target, (col, row), app, ui);
            ui.overlays.push(Overlay::context_menu(menu));
        }
        Action::RightClick { col, row } => {
            if !ui.overlays.is_empty() {
                return Ok(false);
            }
            let hit = ui.hitmap.hit(row, col);
            let target = match hit {
                Some(HitTarget::SidebarRow(idx)) | Some(HitTarget::SidebarToggle(idx)) => {
                    ui.focus = Focus::Sidebar;
                    ui.sidebar_selected = idx;
                    app.snap_sidebar_selection(ui);
                    ContextTarget::Sidebar
                }
                Some(HitTarget::Pane(Focus::Sidebar)) => {
                    ui.focus = Focus::Sidebar;
                    app.snap_sidebar_selection(ui);
                    ContextTarget::Sidebar
                }
                Some(HitTarget::CanvasBlock(idx))
                | Some(HitTarget::TodoToggle(idx))
                | Some(HitTarget::ToggleBlock(idx))
                | Some(HitTarget::ChildCard(idx))
                | Some(HitTarget::MediaChip(idx))
                | Some(HitTarget::NotionButton(idx))
                | Some(HitTarget::CodeHtmlPreview(idx)) => {
                    ui.focus = Focus::Canvas;
                    ui.selected_block = idx;
                    app.clamp_canvas_selection(ui);
                    if app.active_view == CanvasView::Database {
                        ContextTarget::Database
                    } else {
                        ContextTarget::PageCanvas
                    }
                }
                Some(HitTarget::InlineEditorBody) => {
                    ui.focus = Focus::Canvas;
                    ContextTarget::PageCanvas
                }
                Some(HitTarget::DbRow(idx)) => {
                    ui.focus = Focus::Canvas;
                    ui.db_selected_row = idx;
                    app.clamp_db_selection(ui);
                    ContextTarget::Database
                }
                Some(HitTarget::DbBoardCard { col: c, row: r }) => {
                    ui.focus = Focus::Canvas;
                    ui.db_board_col = c;
                    ui.db_board_row = r;
                    app.clamp_db_selection(ui);
                    ContextTarget::Database
                }
                Some(HitTarget::DbBoardColumn(c)) => {
                    ui.focus = Focus::Canvas;
                    ui.db_board_col = c;
                    app.clamp_db_selection(ui);
                    ContextTarget::Database
                }
                Some(HitTarget::Pane(Focus::Canvas)) | Some(HitTarget::DbViewTab(_)) => {
                    ui.focus = Focus::Canvas;
                    if app.active_view == CanvasView::Database {
                        ContextTarget::Database
                    } else {
                        ContextTarget::PageCanvas
                    }
                }
                _ => match ui.focus {
                    Focus::Sidebar => {
                        app.snap_sidebar_selection(ui);
                        ContextTarget::Sidebar
                    }
                    Focus::Canvas if app.active_view == CanvasView::Database => {
                        ContextTarget::Database
                    }
                    Focus::Canvas => ContextTarget::PageCanvas,
                    _ => ContextTarget::Global,
                },
            };
            let menu = ContextMenu::build(target, (col, row), app, ui);
            ui.overlays.push(Overlay::context_menu(menu));
        }
        Action::OpenLinkExtractor => {
            if app.active_view != CanvasView::Page {
                ui.info("Open a Notion page to extract links");
                return Ok(false);
            }
            if let Some(detail) = app.detail_for_block_action(ui) {
                ui.overlays
                    .push(Overlay::links(LinkExtractorOverlay::from_notion_page(&detail)));
            } else {
                ui.info("Open a Notion page to extract links");
            }
        }
        Action::OpenImagePreview => {
            if app.active_view != CanvasView::Page {
                ui.info("Open a Notion page with images first");
                return Ok(false);
            }
            if let Some(detail) = app.detail_for_block_action(ui) {
                let visible = AppState::visible_detail_blocks(&detail, ui);
                if let Some(vblk) = visible.get(ui.selected_block) {
                    match &vblk.block.kind {
                        NotionBlockKind::Code {
                            rich_text,
                            language,
                            ..
                        } if is_html_code_language(language) => {
                            let html = spans_plain_text(rich_text);
                            let title = format!("{} ({language})", detail.page.display_title());
                            drop(detail);
                            return Ok(dispatch(Action::OpenHtmlPreview { title, html }, app, ui));
                        }
                        NotionBlockKind::File { url, name, .. }
                            if is_previewable_markup_ref(url, name.as_deref()) =>
                        {
                            let fallback = if is_markdown_file_ref(url, name.as_deref()) {
                                "Markdown File"
                            } else {
                                "HTML File"
                            };
                            let title = name
                                .clone()
                                .filter(|s| !s.trim().is_empty())
                                .unwrap_or_else(|| fallback.to_string());
                            let u = url.clone();
                            let bid = vblk.block.id.clone();
                            let pid = Some(detail.page.id.clone());
                            drop(detail);
                            return Ok(dispatch(
                                Action::FetchHtmlPreview {
                                    title,
                                    url: u,
                                    block_id: (!bid.is_empty()).then_some(bid),
                                    page_id: pid,
                                },
                                app,
                                ui,
                            ));
                        }
                        NotionBlockKind::Image { url, caption, .. }
                            if is_html_or_xml_ref(url, None) =>
                        {
                            let cap = spans_plain_text(caption);
                            let title = if cap.trim().is_empty() {
                                "HTML Preview".to_string()
                            } else {
                                cap
                            };
                            let u = url.clone();
                            let bid = vblk.block.id.clone();
                            let pid = Some(detail.page.id.clone());
                            drop(detail);
                            return Ok(dispatch(
                                Action::FetchHtmlPreview {
                                    title,
                                    url: u,
                                    block_id: (!bid.is_empty()).then_some(bid),
                                    page_id: pid,
                                },
                                app,
                                ui,
                            ));
                        }
                        _ => {}
                    }
                }
                let preferred_media_idx = visible.get(ui.selected_block).and_then(|vblk| {
                    match &vblk.block.kind {
                        NotionBlockKind::Image { url, .. } | NotionBlockKind::File { url, .. } => {
                            find_media_index_for_block(&detail, &vblk.block.id, url)
                        }
                        _ => None,
                    }
                });
                let overlay = ImagePreviewOverlay::from_notion_page(&detail, preferred_media_idx);
                drop(detail);
                if let Some(ov) = overlay
                {
                    ui.overlays.push(Overlay::ImagePreview(ov));
                    return Ok(dispatch(Action::LoadPreviewImage, app, ui));
                } else {
                    ui.info("No images on the current page");
                }
            } else {
                ui.info("Open a Notion page with images first");
            }
        }
        Action::OpenImagePreviewAt(media_idx) => {
            if app.active_view != CanvasView::Page {
                ui.info("Open a Notion page with images first");
                return Ok(false);
            }
            if let Some(detail) = app.detail_for_block_action(ui) {
                let overlay = ImagePreviewOverlay::from_notion_page(&detail, Some(media_idx));
                drop(detail);
                if let Some(ov) = overlay {
                    ui.overlays.push(Overlay::ImagePreview(ov));
                    return Ok(dispatch(Action::LoadPreviewImage, app, ui));
                } else {
                    ui.info("No images on the current page");
                }
            } else {
                ui.info("Open a Notion page with images first");
            }
        }
        Action::OpenHtmlPreview { title, html } => {
            ui.bump_preview_request_seq();
            if matches!(
                ui.overlays.last(),
                Some(Overlay::ImagePreview(_) | Overlay::HtmlPreview(_))
            ) {
                ui.overlays.pop();
                ui.active_pixel_graphic = None;
                ui.pending_pixel_graphics.clear();
            }
            ui.overlays.push(Overlay::html_preview(title, html));
        }
        Action::FetchHtmlPreview {
            title,
            url,
            block_id,
            page_id,
        } => {
            if url.trim().is_empty() {
                ui.warning("HTML file URL is empty");
                return Ok(false);
            }
            let req_seq = ui.bump_preview_request_seq();
            let client = app.client.clone();
            let http = client.http_client().clone();
            let ui_tx = app.ui_tx.clone();
            let pid = page_id.unwrap_or_default();
            ui.tasks.begin("html");
            tokio::spawn(async move {
                let fetched = match media::fetch_media_bytes_cached(&http, &url).await {
                    Ok(ok) => Ok(ok),
                    Err(first_err) => {
                        if let Some(bid) = block_id.filter(|s| !s.trim().is_empty()) {
                            if let Ok(blk) = client.get_block(&bid, &pid).await {
                                if let Some(fresh) = media::extract_block_media_url(&blk.kind) {
                                    if fresh != url {
                                        let _ = ui_tx.send(UiMsg::MediaUrlRefreshed {
                                            page_id: pid.clone(),
                                            block_id: bid.clone(),
                                            url: fresh.clone(),
                                        });
                                        media::fetch_media_bytes_cached(&http, &fresh).await
                                    } else {
                                        Err(first_err)
                                    }
                                } else {
                                    Err(first_err)
                                }
                            } else {
                                Err(first_err)
                            }
                        } else {
                            Err(first_err)
                        }
                    }
                };
                match fetched {
                    Ok((bytes, mime)) => {
                        let effective_mime = if sniff_content_kind(
                            Some(&mime),
                            Some(&title),
                            Some(&bytes),
                        ) == ContentKind::Markdown
                            || (sniff_content_kind(None, Some(&url), None) == ContentKind::Markdown
                                && !bitefoundry_html::looks_like_html_bytes(&bytes)
                                && !bitefoundry_html::looks_like_svg_bytes(&bytes))
                        {
                            Some("text/markdown".to_string())
                        } else if mime.trim().is_empty() {
                            None
                        } else {
                            Some(mime)
                        };
                        let html = String::from_utf8_lossy(&bytes).into_owned();
                        let _ = ui_tx.send(UiMsg::HtmlPreviewLoaded {
                            seq: req_seq,
                            title,
                            html,
                            mime: effective_mime,
                        });
                    }
                    Err(err) => {
                        let _ = ui_tx.send(UiMsg::TaskFailed {
                            label: "html".into(),
                            error: err.to_string(),
                        });
                    }
                }
            });
        }
        Action::ServeActiveHtml => {
            if let Some(Overlay::HtmlPreview(ov)) = ui.overlays.last() {
                let title = ov.title.clone();
                let raw_html = ov.session.serve_html();
                let server = app.html_server.clone();
                let ui_tx = app.ui_tx.clone();
                ui.tasks.begin("html-serve");
                tokio::spawn(async move {
                    match server.serve_html(&title, &raw_html).await {
                        Ok(url) => {
                            let _ = ui_tx.send(UiMsg::HtmlServed { url });
                        }
                        Err(err) => {
                            let _ = ui_tx.send(UiMsg::TaskFailed {
                                label: "html-serve".into(),
                                error: err.to_string(),
                            });
                        }
                    }
                });
            }
        }
        Action::ShowToast(msg) => {
            ui.info(msg);
        }
        Action::OpenActiveInBrowser => {
            let url = match app.active_view {
                CanvasView::Page => app
                    .active_page_detail
                    .as_ref()
                    .map(|d| d.page.url.clone()),
                CanvasView::Database => app
                    .active_database()
                    .map(|d| d.url.clone()),
            };
            if let Some(u) = url.filter(|s| !s.trim().is_empty()) {
                return Ok(dispatch(Action::OpenUrl(u), app, ui));
            } else {
                ui.warning("No Notion URL available for the current item");
            }
        }
        Action::OpenUrl(url) => match open_url_in_browser(&url) {
            Ok(()) => ui.info(format!("Opened {url}")),
            Err(err) => ui.error(format!("Failed to open browser: {err}")),
        },
        Action::OpenMediaUrl {
            url,
            block_id,
            page_id,
        } => {
            let bid = block_id.filter(|s| !s.trim().is_empty());
            if media::is_expiring_notion_s3_url(&url) {
                if let Some(bid) = bid {
                    let client = app.client.clone();
                    let ui_tx = app.ui_tx.clone();
                    let pid = page_id
                        .or_else(|| app.active_page_id.clone())
                        .unwrap_or_default();
                    ui.tasks.begin("open-media");
                    tokio::spawn(async move {
                        let mut target_url = url.clone();
                        if let Ok(blk) = client.get_block(&bid, &pid).await {
                            if let Some(fresh_url) = media::extract_block_media_url(&blk.kind) {
                                let _ = ui_tx.send(UiMsg::MediaUrlRefreshed {
                                    page_id: pid.clone(),
                                    block_id: bid.clone(),
                                    url: fresh_url.clone(),
                                });
                                target_url = fresh_url;
                            }
                        }
                        match open_url_in_browser(&target_url) {
                            Ok(()) => {
                                let _ = ui_tx.send(UiMsg::TaskDone {
                                    label: "open-media".into(),
                                    toast: format!("Opened {target_url}"),
                                });
                            }
                            Err(err) => {
                                let _ = ui_tx.send(UiMsg::TaskFailed {
                                    label: "open-media".into(),
                                    error: format!("Failed to open browser: {err}"),
                                });
                            }
                        }
                    });
                    return Ok(false);
                }
            }
            return Ok(dispatch(Action::OpenUrl(url), app, ui));
        }
        Action::CopyUrlToClipboard(url) => copy_to_clipboard(&url, "Copied URL to clipboard", ui),
        Action::CopySelectionOrMarkdown => {
            if app.active_view == CanvasView::Database {
                if let Some(row) = selected_db_row_page(app, ui) {
                    let clip = if !row.url.trim().is_empty() {
                        row.url.clone()
                    } else {
                        row.display_title().to_string()
                    };
                    copy_to_clipboard(&clip, "Copied database item to clipboard", ui);
                } else {
                    ui.info("Select a database item to copy");
                }
                return Ok(false);
            }
            if let Some(detail) = app.detail_for_block_action(ui) {
                let visible = AppState::visible_detail_blocks(&detail, ui);
                if let Some(vblk) = visible.get(ui.selected_block) {
                    if let NotionBlockKind::Code { ref rich_text, .. } = vblk.block.kind {
                        let code = crate::models::spans_plain_text(rich_text);
                        copy_to_clipboard(&code, "Copied code block to clipboard", ui);
                        return Ok(false);
                    }
                }
                let md = format!("# {}\n\n{}", detail.page.display_title(), detail.markdown);
                copy_to_clipboard(&md, "Copied page Markdown to clipboard", ui);
            }
        }
        Action::PasteClipboard => {
            if let Ok(mut cb) = arboard::Clipboard::new() {
                if let Ok(text) = cb.get_text() {
                    if !text.trim().is_empty() {
                        if ui.focus == Focus::Comments
                            && ui.comments_drawer_open
                            && app.active_view == CanvasView::Page
                        {
                            ui.comment_reply_input.push_str(text.trim());
                        } else if app.active_view == CanvasView::Page
                            && app.active_page_id.is_some()
                        {
                            return Ok(dispatch(Action::SubmitSlashAppend(text), app, ui));
                        } else {
                            ui.warning("Open a page before pasting blocks");
                        }
                    }
                } else if let Ok(img) = cb.get_image() {
                    let ts = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or(0);
                    match crate::ui::composer::save_clipboard_rgba_to_temp_png(
                        &std::env::temp_dir(),
                        ts,
                        img.width as u32,
                        img.height as u32,
                        img.bytes.into_owned(),
                    ) {
                        Ok((path, _)) => {
                            ui.success(format!("Saved clipboard image -> {}", path.display()));
                        }
                        Err(err) => ui.error(err),
                    }
                }
            }
        }
        Action::AttachFormFile(path) => {
            if let Ok(content) = std::fs::read_to_string(&path) {
                let blocks = parse_slash_command_blocks(&content);
                if !blocks.is_empty()
                    && app.active_view == CanvasView::Page
                    && app.active_page_id.is_some()
                {
                    return Ok(dispatch(Action::SubmitSlashAppend(content), app, ui));
                }
            }
            ui.info(format!("Selected file: {}", path.display()));
        }
        Action::LoadPreviewImage => {
            let req_seq = ui.bump_preview_request_seq();
            if let Some(Overlay::ImagePreview(ov)) = ui.overlays.last() {
                if let Some(target) = ov.current().cloned() {
                    let fit = ov.fit_box(ui.term_width, ui.term_height);
                    let caps = ui.graphics_caps;
                    let bg_rgb = crate::img_preview::color_to_rgb(ui.theme.bg_elevated, (25, 25, 25));
                    let cache_key = ov.cache_key(&target, ui.term_width, ui.term_height, caps);
                    if !app.image_cache.contains_key(&cache_key) {
                        let client = app.client.clone();
                        let http = client.http_client().clone();
                        let ui_tx = app.ui_tx.clone();
                        let url = target.url.clone();
                        let block_id = if !target.block_id.trim().is_empty() {
                            target.block_id.clone()
                        } else {
                            app.active_page_detail
                                .as_ref()
                                .and_then(|d| d.media.get(target.media_index))
                                .map(|m| m.block_id.clone())
                                .unwrap_or_default()
                        };
                        let page_id = target.page_id.clone();
                        tokio::spawn(async move {
                            // First attempt cached/direct URL; if expired (Notion S3 URLs expire after 1h),
                            // refresh the block from Notion API to get a fresh signed URL and retry once.
                            let fetched = match media::fetch_media_bytes_cached(&http, &url).await {
                                Ok(ok) => Ok(ok),
                                Err(first_err) if !block_id.trim().is_empty() => {
                                    if let Ok(blk) = client.get_block(&block_id, &page_id).await {
                                        if let Some(fresh_url) =
                                            media::extract_block_media_url(&blk.kind)
                                        {
                                            if fresh_url != url {
                                                let _ = ui_tx.send(UiMsg::MediaUrlRefreshed {
                                                    page_id: page_id.clone(),
                                                    block_id: block_id.clone(),
                                                    url: fresh_url.clone(),
                                                });
                                                media::fetch_media_bytes_cached(&http, &fresh_url)
                                                    .await
                                            } else {
                                                Err(first_err)
                                            }
                                        } else {
                                            Err(first_err)
                                        }
                                    } else {
                                        Err(first_err)
                                    }
                                }
                                Err(err) => Err(err),
                            };

                            let display_name = target.display_name.clone();
                            match fetched {
                                Ok((bytes, mime)) => {
                                    let is_xml = mime == "application/xml"
                                        || mime == "text/xml"
                                        || media::looks_like_xml_bytes(&bytes);
                                    if is_xml {
                                        let xml_src = String::from_utf8_lossy(&bytes).into_owned();
                                        let _ = ui_tx.send(UiMsg::HtmlPreviewLoaded {
                                            seq: req_seq,
                                            title: display_name,
                                            html: xml_src,
                                            mime: Some(mime),
                                        });
                                    } else {
                                        match sniff_content_kind(
                                            Some(&mime),
                                            Some(&display_name),
                                            Some(&bytes),
                                        ) {
                                            ContentKind::Html | ContentKind::Markdown => {
                                                let html =
                                                    String::from_utf8_lossy(&bytes).into_owned();
                                                let _ = ui_tx.send(UiMsg::HtmlPreviewLoaded {
                                                    seq: req_seq,
                                                    title: display_name,
                                                    html,
                                                    mime: Some(mime),
                                                });
                                            }
                                            _ => {
                                                let rendered = tokio::task::spawn_blocking(move || {
                                                    crate::img_preview::decode_and_render_for_protocol(
                                                        &bytes,
                                                        fit,
                                                        bg_rgb,
                                                        caps,
                                                    )
                                                })
                                                .await;
                                                match rendered {
                                                    Ok(Ok(img)) => {
                                                        let _ = ui_tx.send(UiMsg::ImageRendered {
                                                            cache_key,
                                                            rendered: Box::new(img),
                                                        });
                                                    }
                                                    Ok(Err(err)) => {
                                                        let _ = ui_tx.send(
                                                            UiMsg::ImagePreviewFailed {
                                                                seq: req_seq,
                                                                error: format!(
                                                                    "{err} (press 'o' to open in browser)"
                                                                ),
                                                            },
                                                        );
                                                    }
                                                    Err(join_err) => {
                                                        let _ = ui_tx.send(
                                                            UiMsg::ImagePreviewFailed {
                                                                seq: req_seq,
                                                                error: format!(
                                                                    "Image decode task failed: {join_err}"
                                                                ),
                                                            },
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                Err(err) => {
                                    let _ = ui_tx.send(UiMsg::ImagePreviewFailed {
                                        seq: req_seq,
                                        error: format!("{err} (press 'o' to open in browser)"),
                                    });
                                }
                            }
                        });
                    }
                }
            }
        }
        Action::CycleImageProtocol => {
            let next = ui.graphics_caps.protocol.next_manual();
            ui.graphics_caps.protocol = next;
            app.config.image_protocol = match next {
                crate::img_preview::TerminalImageProtocol::Kitty => ImageProtocolPreference::Kitty,
                crate::img_preview::TerminalImageProtocol::ITerm2 => {
                    ImageProtocolPreference::Iterm2
                }
                crate::img_preview::TerminalImageProtocol::Sixel => ImageProtocolPreference::Sixel,
                crate::img_preview::TerminalImageProtocol::Halfblock => {
                    ImageProtocolPreference::Halfblock
                }
            };
            let _ = app.config.save();
            ui.active_pixel_graphic = None;
            ui.info(format!("Image protocol: {}", next.label()));
            if matches!(ui.overlays.last(), Some(Overlay::ImagePreview(_))) {
                return Ok(dispatch(Action::LoadPreviewImage, app, ui));
            }
        }
        Action::DownloadMedia(media_idx) => {
            if let Some(detail) = app.detail_for_block_action(ui) {
                if let Some(m) = detail.media.get(media_idx).cloned() {
                    let cfg = app.config.clone();
                    let client = app.client.clone();
                    let ui_tx = app.ui_tx.clone();
                    ui.tasks.begin("download");
                    tokio::spawn(async move {
                        match media::download_media_target(&cfg, &client, &m.url, None).await {
                            Ok(dl) => {
                                let _ = ui_tx.send(UiMsg::TaskDone {
                                    label: "download".into(),
                                    toast: format!("Saved {}", dl.path),
                                });
                            }
                            Err(err) => {
                                let _ = ui_tx.send(UiMsg::TaskFailed {
                                    label: "download".into(),
                                    error: err.to_string(),
                                });
                            }
                        }
                    });
                }
            }
        }
        other => return Err(other),
    }
    Ok(false)
}

pub(crate) fn is_html_code_language(language: &str) -> bool {
    let lower = language.trim().to_ascii_lowercase();
    matches!(lower.as_str(), "html" | "xml" | "xhtml")
}

pub(crate) fn is_html_file_ref(url: &str, name: Option<&str>) -> bool {
    name.is_some_and(|n| sniff_content_kind(None, Some(n), None) == ContentKind::Html)
        || sniff_content_kind(None, Some(url), None) == ContentKind::Html
}

pub(crate) fn is_markdown_file_ref(url: &str, name: Option<&str>) -> bool {
    name.is_some_and(|n| sniff_content_kind(None, Some(n), None) == ContentKind::Markdown)
        || sniff_content_kind(None, Some(url), None) == ContentKind::Markdown
}

pub(super) fn is_previewable_markup_ref(url: &str, name: Option<&str>) -> bool {
    is_html_or_xml_ref(url, name) || is_markdown_file_ref(url, name)
}

pub(super) fn is_html_or_xml_ref(url: &str, name: Option<&str>) -> bool {
    if is_html_file_ref(url, name) {
        return true;
    }
    let check_ref = |s: &str| {
        let lower = s
            .split(['?', '#'])
            .next()
            .unwrap_or(s)
            .to_ascii_lowercase();
        lower.ends_with(".xml")
    };
    name.is_some_and(check_ref) || check_ref(url)
}

