use axum::http::StatusCode;
use ratatui::{backend::TestBackend, Terminal};
use serde_json::{json, Value};

use notion_cli::{
    mcp,
    notion::media,
    serve,
    storage::Storage,
    sync,
    ui::{
        self,
        app::{AppState, UiState},
    },
};

use crate::audit_mock::{
    audit_block, audit_database, audit_limit_violation, audit_page, audit_title_of,
    start_audit_api, start_audit_env, AuditEnv, AUDIT_DB, AUDIT_PAGE_A, AUDIT_PAGE_B,
};

#[tokio::test]
async fn regression_s01_serve_rejects_cross_origin_and_foreign_host() {
    let env = start_audit_env().await;
    env.mock.add_page(audit_page(
        AUDIT_PAGE_A,
        "Launch Plan",
        json!({ "type": "workspace", "workspace": true }),
        "2026-11-20T02:00:00.000Z",
    ));
    let (api, api_server) = start_audit_api(&env, Some("local-secret"), serve::ServeOptions::default()).await;
    let http = reqwest::Client::new();
    let forbidden = |resp: &reqwest::Response| {
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(
            resp.headers().get("access-control-allow-origin").is_none(),
            "a rejected request must not carry CORS headers"
        );
    };

    // 1. Preflight from a foreign origin: rejected, and never answered with CORS headers.
    let preflight = http
        .request(reqwest::Method::OPTIONS, format!("{api}/api/v1/pages"))
        .header("Origin", "https://evil.example")
        .header("Access-Control-Request-Method", "POST")
        .header("Access-Control-Request-Headers", "content-type,x-api-key")
        .send()
        .await
        .unwrap();
    forbidden(&preflight);
    assert!(preflight.headers().get("access-control-allow-methods").is_none());

    // 2. A simple cross-origin GET, even with the right key.
    let cross_get = http
        .get(format!("{api}/api/v1/search?q=x"))
        .header("Origin", "https://evil.example")
        .header("x-api-key", "local-secret")
        .send()
        .await
        .unwrap();
    forbidden(&cross_get);
    let body: Value = cross_get.json().await.unwrap();
    assert_eq!(body, json!({ "ok": false, "error": "Forbidden origin" }));

    // 3. DNS rebinding: the socket is loopback but the Host header names another site.
    for host in ["evil.example", "127.0.0.1.evil.example:8791", "localhost.evil.example"] {
        let rebound = http
            .get(format!("{api}/api/v1/whoami"))
            .header("Host", host)
            .header("x-api-key", "local-secret")
            .send()
            .await
            .unwrap();
        forbidden(&rebound);
        let body: Value = rebound.json().await.unwrap();
        assert_eq!(body, json!({ "ok": false, "error": "Forbidden host" }), "{host}");
    }
    let health_rebound = http
        .get(format!("{api}/health"))
        .header("Host", "evil.example")
        .send()
        .await
        .unwrap();
    forbidden(&health_rebound);

    // 4. A body-less cross-site POST needs no preflight; it must not reach Notion.
    let searches_before = env.mock.count("POST", "/v1/search");
    for site in ["cross-site", "same-site"] {
        let cross_post = http
            .post(format!("{api}/api/v1/sync"))
            .header("Sec-Fetch-Site", site)
            .header("x-api-key", "local-secret")
            .send()
            .await
            .unwrap();
        forbidden(&cross_post);
    }
    assert_eq!(env.mock.count("POST", "/v1/search"), searches_before);
    assert!(env.mock.requests.lock().unwrap().is_empty(), "no guarded request may reach Notion");

    // 5. The same calls from a local client with the key work, with no CORS headers at all.
    let health = http.get(format!("{api}/health")).send().await.unwrap();
    assert_eq!(health.status(), StatusCode::OK);
    let missing_key = http.get(format!("{api}/api/v1/whoami")).send().await.unwrap();
    assert_eq!(missing_key.status(), StatusCode::UNAUTHORIZED);
    let wrong_key = http
        .get(format!("{api}/api/v1/whoami"))
        .header("Authorization", "Bearer local-secreT")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_key.status(), StatusCode::UNAUTHORIZED);
    let whoami = http
        .get(format!("{api}/api/v1/whoami"))
        .header("Authorization", "Bearer local-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(whoami.status(), StatusCode::OK);
    assert!(whoami.headers().get("access-control-allow-origin").is_none());
    let sync = http
        .post(format!("{api}/api/v1/sync"))
        .header("x-api-key", "local-secret")
        .header("Sec-Fetch-Site", "none")
        .send()
        .await
        .unwrap();
    assert_eq!(sync.status(), StatusCode::OK);
    let sync_body: Value = sync.json().await.unwrap();
    assert_eq!(sync_body["pages_count"], 1);
    api_server.abort();

    // 6. --cors-origin opts in exactly one origin; --allowed-host one extra Host name.
    let (cors_api, cors_server) = start_audit_api(
        &env,
        Some("local-secret"),
        serve::ServeOptions {
            allowed_hosts: vec!["notion.internal".into()],
            cors_origins: vec!["https://app.example.com".into()],
            enforce_host: true,
        },
    )
    .await;
    let allowed_preflight = http
        .request(reqwest::Method::OPTIONS, format!("{cors_api}/api/v1/pages"))
        .header("Origin", "https://app.example.com")
        .header("Access-Control-Request-Method", "POST")
        .header("Access-Control-Request-Headers", "content-type,x-api-key")
        .send()
        .await
        .unwrap();
    assert!(allowed_preflight.status().is_success());
    assert_eq!(
        allowed_preflight.headers()["access-control-allow-origin"],
        "https://app.example.com"
    );
    let allowed_get = http
        .get(format!("{cors_api}/api/v1/whoami"))
        .header("Origin", "https://app.example.com")
        .header("Sec-Fetch-Site", "cross-site")
        .header("Host", "notion.internal:8791")
        .header("x-api-key", "local-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(allowed_get.status(), StatusCode::OK);
    assert_eq!(
        allowed_get.headers()["access-control-allow-origin"],
        "https://app.example.com"
    );
    // The opt-in does not remove the key requirement.
    let allowed_origin_no_key = http
        .get(format!("{cors_api}/api/v1/whoami"))
        .header("Origin", "https://app.example.com")
        .send()
        .await
        .unwrap();
    assert_eq!(allowed_origin_no_key.status(), StatusCode::UNAUTHORIZED);
    for origin in ["https://evil.example", "https://app.example.com.evil.example", "null"] {
        let other = http
            .get(format!("{cors_api}/api/v1/whoami"))
            .header("Origin", origin)
            .header("x-api-key", "local-secret")
            .send()
            .await
            .unwrap();
        forbidden(&other);
    }

    cors_server.abort();
    env.server.abort();
}

#[tokio::test]
async fn regression_n14_peek_url_archives_row_not_parent() {
    let env = start_audit_env().await;
    env.mock.databases.lock().unwrap().push(audit_database(
        AUDIT_DB,
        "Sprint Tasks",
        "2026-11-20T02:00:00.000Z",
    ));
    for (id, title) in [(AUDIT_PAGE_A, "Row A"), (AUDIT_PAGE_B, "Row B")] {
        env.mock.add_page(audit_page(
            id,
            title,
            json!({ "type": "database_id", "database_id": AUDIT_DB }),
            "2026-11-20T02:00:00.000Z",
        ));
    }
    let compact = |id: &str| id.replace('-', "");
    let view = "00000000000000000000000000000099";

    // A side-peek URL: the path is the database, p= is the row that is open.
    let peek = format!(
        "https://www.notion.so/acme/{}?v={view}&p={}&pm=s",
        compact(AUDIT_DB),
        compact(AUDIT_PAGE_B)
    );
    let archived = notion_cli::cli::op_archive_page(&env.client, &env.storage, &peek, true)
        .await
        .unwrap();
    assert_eq!(archived.id, AUDIT_PAGE_B);
    let patches = env.mock.requests_matching("PATCH", "/v1/pages/");
    assert_eq!(patches.len(), 1, "{patches:?}");
    assert_eq!(patches[0].path, format!("/v1/pages/{AUDIT_PAGE_B}"));
    assert_eq!(patches[0].body, json!({ "archived": true }));
    assert_eq!(env.mock.page(AUDIT_PAGE_B).unwrap()["archived"], true);
    assert_eq!(env.mock.page(AUDIT_PAGE_A).unwrap()["archived"], false);
    assert_eq!(env.mock.count("POST", "/v1/search"), 0, "URL input needs no lookup");

    // Database commands given the same URL still target the database in the path.
    env.mock.clear_requests();
    let rows = notion_cli::cli::op_query_database(
        &env.client,
        &env.storage,
        &peek,
        None,
        None,
        20,
        None,
        false,
    )
    .await
    .unwrap();
    assert_eq!(rows.items.len(), 1, "the archived row is gone, the other remains");
    assert_eq!(env.mock.count("POST", &format!("/v1/databases/{AUDIT_DB}/query")), 1);

    // Trailing slash and hyphenated UUID forms resolve to the page in the path.
    env.mock.clear_requests();
    for url in [
        format!("https://www.notion.so/acme/Row-A-{}/", compact(AUDIT_PAGE_A)),
        format!("https://www.notion.so/acme/Row-A-{AUDIT_PAGE_A}?pvs=4#top"),
    ] {
        let page = notion_cli::cli::op_archive_page(&env.client, &env.storage, &url, true)
            .await
            .unwrap();
        assert_eq!(page.id, AUDIT_PAGE_A, "{url}");
    }
    assert_eq!(env.mock.count("PATCH", &format!("/v1/pages/{AUDIT_PAGE_A}")), 2);
    env.server.abort();
}

/// Joins the text.content of every span in a recorded rich_text array.
fn audit_joined(spans: &Value) -> String {
    spans
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|s| s.pointer("/text/content").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default()
}

/// Asserts every span of a recorded rich_text array is within the limit and that together they
/// hold `expected` exactly. Returns the span count.
fn assert_within_limit(spans: &Value, expected: &str, what: &str) -> usize {
    let items = spans.as_array().unwrap_or_else(|| panic!("{what}: not an array: {spans}"));
    for item in items {
        let units = item["text"]["content"].as_str().unwrap().encode_utf16().count();
        assert!(units <= 2000, "{what}: span has {units} UTF-16 units");
    }
    assert_eq!(audit_joined(spans), expected, "{what}: text must survive unchanged");
    items.len()
}

/// Like [`assert_within_limit`], for a write that starts from a single span.
fn assert_chunked(spans: &Value, expected: &str, count: usize, what: &str) {
    assert_eq!(assert_within_limit(spans, expected, what), count, "{what}: span count");
}

/// Text of exactly `len` characters built from `unit`, ending in a non-space character.
fn audit_long_text(unit: &str, len: usize) -> String {
    let mut text: String = unit.chars().cycle().take(len).collect();
    if text.ends_with(char::is_whitespace) {
        text.pop();
        text.push('.');
    }
    text
}

#[tokio::test]
async fn regression_n03_long_text_is_chunked_on_every_write_path() {
    const ROW: &str = "a11d0000-0000-4000-8000-0000000000a1";
    const CODE_BLOCK: &str = "a11d0000-0000-4000-8000-0000000000c0";
    const PARAGRAPH: &str = "a11d0000-0000-4000-8000-0000000000c1";

    let env = start_audit_env().await;
    env.mock.databases.lock().unwrap().push(audit_database(
        AUDIT_DB,
        "Sprint Tasks",
        "2026-11-20T02:00:00.000Z",
    ));
    for (id, title) in [(AUDIT_PAGE_A, "Long Notes"), (AUDIT_PAGE_B, "Edit Target")] {
        env.mock.add_page(audit_page(
            id,
            title,
            json!({ "type": "workspace", "workspace": true }),
            "2026-11-20T02:00:00.000Z",
        ));
    }
    env.mock.add_page(audit_page(
        ROW,
        "Row",
        json!({ "type": "database_id", "database_id": AUDIT_DB }),
        "2026-11-20T02:00:00.000Z",
    ));
    let mut code_block = audit_block(CODE_BLOCK, "code", "let old = 1;");
    code_block["code"]["language"] = json!("rust");
    env.mock.set_children(
        AUDIT_PAGE_B,
        vec![audit_block(PARAGRAPH, "paragraph", "short paragraph"), code_block],
    );

    // The mock refuses what Notion refuses, so a pass below means the request was valid.
    let oversized = json!({ "children": [{ "paragraph": { "rich_text": [{ "text": { "content": "x".repeat(2001) } }] } }] });
    assert!(audit_limit_violation(&oversized).is_some());

    // 1. Append a 5,000-character fenced code block.
    let code = audit_long_text("let total = compute(input) + offset;\n", 5000);
    let appended = notion_cli::cli::op_append_blocks(
        &env.client,
        &env.storage,
        AUDIT_PAGE_A,
        &format!("```rust\n{code}\n```"),
    )
    .await
    .unwrap();
    assert_eq!(appended.len(), 1);
    let appends = env.mock.requests_matching("PATCH", &format!("/v1/blocks/{AUDIT_PAGE_A}/children"));
    assert_eq!(appends.len(), 1, "{appends:?}");
    assert_chunked(&appends[0].body["children"][0]["code"]["rich_text"], &code, 3, "appended code");
    let detail = notion_cli::cli::op_get_page(&env.client, &env.storage, AUDIT_PAGE_A, false)
        .await
        .unwrap();
    assert!(detail.markdown.contains(&code), "code must read back unchanged");

    // 2. A 4,100-character Korean comment.
    let comment_text = audit_long_text("긴 한국어 댓글 본문입니다 ", 4100);
    let comment = notion_cli::cli::op_add_comment(&env.client, &env.storage, AUDIT_PAGE_A, None, &comment_text)
        .await
        .unwrap();
    assert_eq!(comment.body, comment_text);
    let posts = env.mock.requests_matching("POST", "/v1/comments");
    assert_eq!(posts.len(), 1);
    assert_chunked(&posts[0].body["rich_text"], &comment_text, 3, "comment");

    // 3. Create a page with a 2,100-character title and a 2,500-character paragraph.
    let title = audit_long_text("Quarterly planning notes ", 2100);
    let paragraph = audit_long_text("A sentence that keeps going. ", 2500);
    let created = notion_cli::cli::op_create_page(
        &env.client,
        &env.storage,
        AUDIT_PAGE_A,
        &title,
        None,
        Some(&paragraph),
        false,
        None,
    )
    .await
    .unwrap();
    assert_eq!(created.title, title);
    let creates = env.mock.requests_matching("POST", "/v1/pages");
    assert_eq!(creates.len(), 1);
    assert_chunked(&creates[0].body["properties"]["title"]["title"], &title, 2, "page title");
    assert_chunked(
        &creates[0].body["children"][0]["paragraph"]["rich_text"],
        &paragraph,
        2,
        "page body paragraph",
    );

    // 4. Replace a code block's text with 3,000 characters.
    let new_code = audit_long_text("match state { Ready => run(), _ => wait() }\n", 3000);
    notion_cli::cli::op_update_block_text(&env.client, &env.storage, CODE_BLOCK, Some("code"), &new_code, None)
        .await
        .unwrap();
    let block_patches = env.mock.requests_matching("PATCH", &format!("/v1/blocks/{CODE_BLOCK}"));
    assert_eq!(block_patches.len(), 1);
    assert_chunked(&block_patches[0].body["code"]["rich_text"], &new_code, 2, "updated code");

    // 5. Caller-supplied properties JSON with a 2,500-character rich_text value.
    let notes = audit_long_text("Reviewer notes, pasted in full. ", 2500);
    notion_cli::cli::op_update_database_item(
        &env.client,
        &env.storage,
        ROW,
        None,
        None,
        Some(json!({ "Notes": { "rich_text": [{ "text": { "content": notes } }] } })),
    )
    .await
    .unwrap();
    let row_patches = env.mock.requests_matching("PATCH", &format!("/v1/pages/{ROW}"));
    assert_eq!(row_patches.len(), 1);
    assert_chunked(&row_patches[0].body["properties"]["Notes"]["rich_text"], &notes, 2, "row property");

    // 6. Edit a paragraph to 2,600 characters through the Markdown diff sync.
    env.mock.clear_requests();
    let before = notion_cli::cli::op_get_page(&env.client, &env.storage, AUDIT_PAGE_B, false)
        .await
        .unwrap();
    assert!(before.markdown.contains(&new_code), "the 3,000-character code reads back unchanged");
    let long_paragraph = audit_long_text("Edited paragraph text that grows. ", 2600);
    let edited = before.markdown.replace("short paragraph", &long_paragraph);
    env.mock.clear_requests();
    let after = notion_cli::cli::op_diff_sync_page_markdown(&env.client, &env.storage, AUDIT_PAGE_B, &edited, None)
        .await
        .unwrap();
    assert!(after.markdown.contains(&long_paragraph));
    let edits = env.mock.requests_matching("PATCH", "/v1/blocks/");
    assert_eq!(edits.len(), 1, "only the paragraph changes: {edits:?}");
    assert_eq!(edits[0].path, format!("/v1/blocks/{PARAGRAPH}"));
    // The edit keeps the old span on the stretch of new text it aligns with and adds the rest as
    // new spans, so the count depends on that alignment; the limit and the text do not.
    let spans = assert_within_limit(&edits[0].body["paragraph"]["rich_text"], &long_paragraph, "edited paragraph");
    assert!(spans >= 2, "2,600 characters need at least two spans, got {spans}");

    // 7. Text beyond 100 spans fails locally and sends nothing.
    env.mock.clear_requests();
    let huge = "c".repeat(250_000);
    let err = notion_cli::cli::op_append_blocks(
        &env.client,
        &env.storage,
        AUDIT_PAGE_A,
        &format!("```\n{huge}\n```"),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("too long for one Notion block"), "{err}");
    assert_eq!(env.mock.count("PATCH", &format!("/v1/blocks/{AUDIT_PAGE_A}/children")), 0);
    env.server.abort();
}

/// Waits for a workspace tree update that contains `page_id`.
async fn audit_wait_for_tree(
    core_rx: &mut tokio::sync::mpsc::UnboundedReceiver<notion_cli::models::CoreEvent>,
    page_id: &str,
) -> Vec<String> {
    let wait = async {
        loop {
            match core_rx.recv().await {
                Some(notion_cli::models::CoreEvent::WorkspaceTreeUpdated { pages, .. })
                    if pages.iter().any(|p| p.id == page_id) =>
                {
                    return pages.into_iter().map(|p| p.id).collect();
                }
                Some(_) => {}
                None => panic!("sync engine stopped before syncing {page_id}"),
            }
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(20), wait)
        .await
        .unwrap_or_else(|_| panic!("no workspace tree with {page_id} within 20 s"))
}

#[tokio::test]
async fn regression_n01_rebind_sync_writes_the_new_workspace_into_its_own_cache() {
    let env = start_audit_env().await;
    let workspace = json!({ "type": "workspace", "workspace": true });
    env.mock.add_page(audit_page(AUDIT_PAGE_A, "Workspace A Home", workspace.clone(), "2026-11-20T02:00:00.000Z"));
    let dir = tempfile::tempdir().unwrap();
    let db_a = dir.path().join("ws-a").join("cache.db");
    let db_b = dir.path().join("ws-b").join("cache.db");
    let ws_id = env.auth.active_workspace_id().await;
    let cached_ids = |storage: &Storage| -> Vec<String> {
        storage.list_pages(&ws_id, true).unwrap().into_iter().map(|p| p.id).collect()
    };
    let (core_tx, mut core_rx) = tokio::sync::mpsc::unbounded_channel();

    let (engine_a, storage_a) =
        notion_cli::tui::rebind_sync(env.cfg.clone(), env.client.clone(), &db_a, core_tx.clone()).unwrap();
    assert_eq!(audit_wait_for_tree(&mut core_rx, AUDIT_PAGE_A).await, vec![AUDIT_PAGE_A]);
    assert_eq!(cached_ids(&storage_a), vec![AUDIT_PAGE_A]);

    // The user switches workspace: the old engine stops and the API now shows other pages.
    engine_a.request(sync::SyncCommand::Shutdown);
    env.mock.pages.lock().unwrap().clear();
    env.mock.add_page(audit_page(AUDIT_PAGE_B, "Workspace B Home", workspace, "2026-11-21T02:00:00.000Z"));
    let (engine_b, storage_b) =
        notion_cli::tui::rebind_sync(env.cfg.clone(), env.client.clone(), &db_b, core_tx.clone()).unwrap();
    assert_eq!(audit_wait_for_tree(&mut core_rx, AUDIT_PAGE_B).await, vec![AUDIT_PAGE_B]);

    assert_eq!(cached_ids(&storage_b), vec![AUDIT_PAGE_B]);
    assert_eq!(
        cached_ids(&Storage::open(&db_a).unwrap()),
        vec![AUDIT_PAGE_A],
        "the first workspace's cache file is left as it was"
    );
    engine_b.request(sync::SyncCommand::Shutdown);
    env.server.abort();
}

/// Adds `count` rows to the audit mock database, named `Row 0001` and so on.
fn audit_add_rows(env: &AuditEnv, count: usize) -> Vec<String> {
    (0..count)
        .map(|i| {
            let id = format!("a11d0000-0000-4000-8001-{i:012x}");
            env.mock.add_page(audit_page(
                &id,
                &format!("Row {i:04}"),
                json!({ "type": "database_id", "database_id": AUDIT_DB }),
                "2026-11-20T02:00:00.000Z",
            ));
            id
        })
        .collect()
}

async fn audit_wait_for_rows(
    core_rx: &mut tokio::sync::mpsc::UnboundedReceiver<notion_cli::models::CoreEvent>,
) -> (usize, bool) {
    let wait = async {
        loop {
            match core_rx.recv().await {
                Some(notion_cli::models::CoreEvent::DatabaseRowsUpdated { rows, rows_truncated, .. }) => {
                    return (rows.len(), rows_truncated);
                }
                Some(_) => {}
                None => panic!("sync engine stopped before reading the database"),
            }
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(20), wait)
        .await
        .expect("no DatabaseRowsUpdated within 20 s")
}

#[tokio::test]
async fn regression_n04_large_database_reports_truncation_and_reconciles_on_refresh() {
    let env = start_audit_env().await;
    env.mock.databases.lock().unwrap().push(audit_database(
        AUDIT_DB,
        "Big Table",
        "2026-11-20T02:00:00.000Z",
    ));
    let ids = audit_add_rows(&env, 321);
    let stale = ids[0].clone();
    let ws_id = env.auth.active_workspace_id().await;
    let cached = |storage: &Storage| -> Vec<String> {
        storage.list_database_rows(&ws_id, AUDIT_DB).unwrap().into_iter().map(|p| p.id).collect()
    };

    // Cache every row, then archive one in Notion.
    let (_, rows, more) =
        sync::sync_database_rows(&env.client, &env.storage, AUDIT_DB, sync::EXPLICIT_DATABASE_ROW_LIMIT)
            .await
            .unwrap();
    assert_eq!((rows.len(), more), (321, false));
    env.mock.pages.lock().unwrap().iter_mut().find(|p| p["id"] == stale.as_str()).unwrap()["archived"] =
        json!(true);

    // (a) A poll reads the first 250 rows, reports more, and leaves the cache alone.
    let (_, rows, more) =
        sync::sync_database_rows(&env.client, &env.storage, AUDIT_DB, sync::TUI_DATABASE_ROW_LIMIT)
            .await
            .unwrap();
    assert_eq!(rows.len(), 250);
    assert!(more, "320 rows exceed the poll limit");
    assert!(cached(&env.storage).contains(&stale), "a partial read must not archive unseen rows");

    // (b) A full read sees the end of the database and archives the stale row.
    let (_, rows, more) =
        sync::sync_database_rows(&env.client, &env.storage, AUDIT_DB, sync::EXPLICIT_DATABASE_ROW_LIMIT)
            .await
            .unwrap();
    assert_eq!((rows.len(), more), (320, false));
    let after = cached(&env.storage);
    assert_eq!(after.len(), 320);
    assert!(!after.contains(&stale), "the archived row leaves the cache");

    // The engine polls with the small limit and reads everything when the user refreshes.
    let storage = Storage::open_in_memory().unwrap();
    let (core_tx, mut core_rx) = tokio::sync::mpsc::unbounded_channel();
    let engine = sync::spawn_sync_engine(env.cfg.clone(), env.client.clone(), storage, core_tx);
    engine.set_focus(sync::FocusTarget { page_id: None, database_id: Some(AUDIT_DB.into()) });
    assert_eq!(audit_wait_for_rows(&mut core_rx).await, (250, true));
    env.mock.clear_requests();
    engine.request(sync::SyncCommand::RefreshDatabase { database_id: AUDIT_DB.into() });
    assert_eq!(audit_wait_for_rows(&mut core_rx).await, (320, false));
    assert_eq!(env.mock.count("POST", &format!("/v1/databases/{AUDIT_DB}/query")), 4);
    engine.request(sync::SyncCommand::Shutdown);

    // The TUI header says the table is cut off.
    let (focus_tx, _focus_rx) = tokio::sync::watch::channel(sync::FocusTarget::default());
    let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let (ui_tx, _ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let ctrl = sync::SyncController::from_channels(focus_tx, cmd_tx);
    let mut ui_state = UiState::from_config(&env.cfg);
    let mut app = AppState::new(env.cfg.clone(), env.auth.clone(), env.client.clone(), env.storage.clone(), ctrl, ui_tx);
    app.load_cached_initial();
    app.open_database(AUDIT_DB, &mut ui_state, false);
    let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();
    let screen = |terminal: &mut Terminal<TestBackend>, app: &mut AppState, ui_state: &mut UiState| -> String {
        terminal.draw(|f| { ui::draw(f, app, ui_state); }).unwrap();
        terminal.backend().buffer().content.iter().map(|c| c.symbol()).collect()
    };
    assert!(!screen(&mut terminal, &mut app, &mut ui_state).contains("showing first"));
    let first: Vec<_> = env.storage.list_database_rows(&ws_id, AUDIT_DB).unwrap().into_iter().take(250).collect();
    assert!(app.handle_core_event(
        &mut ui_state,
        notion_cli::models::CoreEvent::DatabaseRowsUpdated {
            database_id: AUDIT_DB.into(),
            database: None,
            rows: first,
            rows_truncated: true,
        },
    ));
    assert!(screen(&mut terminal, &mut app, &mut ui_state).contains("250+ rows · showing first 250"));
    env.server.abort();
}

#[tokio::test]
async fn regression_n08_full_flag_reaches_mcp_and_http() {
    let env = start_audit_env().await;
    let workspace = json!({ "type": "workspace", "workspace": true });
    // 100 pages with distinct edit times: the first search batch reaches past the watermark.
    for i in 0..100 {
        env.mock.add_page(audit_page(
            &format!("a11d0000-0000-4000-8008-{i:012x}"),
            &format!("Recent {i:03}"),
            workspace.clone(),
            &format!("2026-11-20T03:{:02}:{:02}.000Z", i / 60, i % 60),
        ));
    }
    let ws_id = env.auth.active_workspace_id().await;
    let cached = |id: &str| env.storage.get_page(&ws_id, id).unwrap().is_some();
    sync::sync_workspace_tree(&env.client, &env.storage).await.unwrap();

    // A page shared with the integration later, edited before the last sync: it ranks 101st.
    let older = "a11d0000-0000-4000-8008-0000000000a1";
    env.mock.add_page(audit_page(older, "Old Shared Page", workspace.clone(), "2026-10-01T00:00:00.000Z"));
    let call = |args: Value| {
        mcp::handle_request_value(
            &env.cfg,
            &env.auth,
            &env.client,
            &env.storage,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": { "name": "sync_now", "arguments": args }
            }),
        )
    };

    // (a) An incremental sync stops at the watermark.
    let resp = call(json!({})).await.unwrap();
    assert_eq!(resp["result"]["isError"], false, "{resp}");
    assert_eq!(resp["result"]["structuredContent"]["full"], false);
    assert!(!cached(older), "incremental sync stops before the older page");

    // (b) full=true reads past it.
    let resp = call(json!({ "full": true })).await.unwrap();
    assert_eq!(resp["result"]["isError"], false, "{resp}");
    assert_eq!(resp["result"]["structuredContent"]["full"], true);
    assert_eq!(resp["result"]["structuredContent"]["fetched_count"], 101);
    assert!(cached(older), "full sync finds the older page");

    // HTTP: another older page, then the same two calls.
    let oldest = "a11d0000-0000-4000-8008-0000000000a2";
    env.mock.add_page(audit_page(oldest, "Oldest Shared Page", workspace, "2026-09-01T00:00:00.000Z"));
    let (api, api_server) = start_audit_api(&env, Some("local-secret"), serve::ServeOptions::default()).await;
    let http = reqwest::Client::new();
    let post = |url: String| http.post(url).header("x-api-key", "local-secret").send();

    // (d) The body-less call keeps working and stays incremental.
    let plain = post(format!("{api}/api/v1/sync")).await.unwrap();
    assert_eq!(plain.status(), StatusCode::OK);
    let body: Value = plain.json().await.unwrap();
    assert_eq!((body["ok"].clone(), body["full"].clone()), (json!(true), json!(false)), "{body}");
    assert!(!cached(oldest));

    // (c) ?full=true rescans.
    let full = post(format!("{api}/api/v1/sync?full=true")).await.unwrap();
    assert_eq!(full.status(), StatusCode::OK);
    let body: Value = full.json().await.unwrap();
    assert_eq!(body["full"], true, "{body}");
    assert_eq!(body["fetched_count"], 102);
    assert!(cached(oldest), "?full=true finds the oldest page");

    let bad = post(format!("{api}/api/v1/sync?full=maybe")).await.unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    api_server.abort();
    env.server.abort();
}

#[tokio::test]
async fn regression_n06_http_database_query_pages_with_cursor() {
    let env = start_audit_env().await;
    env.mock.databases.lock().unwrap().push(audit_database(AUDIT_DB, "Paged", "2026-11-20T02:00:00.000Z"));
    let ids = audit_add_rows(&env, 130);
    let (api, api_server) = start_audit_api(&env, Some("local-secret"), serve::ServeOptions::default()).await;
    let http = reqwest::Client::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let mut body = json!({ "limit": 50 });
        if let Some(c) = &cursor {
            body["cursor"] = json!(c);
        }
        let resp = http
            .post(format!("{api}/api/v1/databases/{AUDIT_DB}/query"))
            .header("x-api-key", "local-secret")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let page: Value = resp.json().await.unwrap();
        assert_eq!(page["ok"], true);
        assert_eq!(page["count"].as_u64().unwrap() as usize, page["rows"].as_array().unwrap().len());
        for row in page["rows"].as_array().unwrap() {
            assert!(seen.insert(row["id"].as_str().unwrap().to_string()), "row repeated: {row}");
        }
        pages += 1;
        if page["has_more"] == true {
            cursor = Some(page["next_cursor"].as_str().expect("has_more comes with a cursor").to_string());
        } else {
            assert!(page["next_cursor"].is_null(), "{page}");
            break;
        }
        assert!(pages < 10, "pagination does not end");
    }
    assert_eq!(pages, 3);
    assert_eq!(seen, ids.into_iter().collect());
    api_server.abort();
    env.server.abort();
}

#[tokio::test]
async fn regression_n13_patch_page_applies_all_fields() {
    let env = start_audit_env().await;
    let workspace = json!({ "type": "workspace", "workspace": true });
    env.mock.add_page(audit_page(AUDIT_PAGE_A, "Draft", workspace, "2026-11-20T02:00:00.000Z"));
    env.mock.databases.lock().unwrap().push(audit_database(AUDIT_DB, "Tasks", "2026-11-20T02:00:00.000Z"));
    env.mock.add_page(audit_page(
        AUDIT_PAGE_B,
        "Row",
        json!({ "type": "database_id", "database_id": AUDIT_DB }),
        "2026-11-20T02:00:00.000Z",
    ));
    let (api, api_server) = start_audit_api(&env, Some("local-secret"), serve::ServeOptions::default()).await;
    let http = reqwest::Client::new();
    let patch = |id: &str, body: Value| {
        http.patch(format!("{api}/api/v1/pages/{id}"))
            .header("x-api-key", "local-secret")
            .json(&body)
            .send()
    };
    let notion_patches = |id: &str| -> Vec<Value> {
        env.mock.requests_matching("PATCH", &format!("/v1/pages/{id}")).into_iter().map(|r| r.body).collect()
    };
    let title_spans = |text: &str| json!([{ "type": "text", "text": { "content": text } }]);

    // (a) title + archived reach Notion in one call.
    let resp = patch(AUDIT_PAGE_A, json!({ "title": "Done", "archived": true })).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        notion_patches(AUDIT_PAGE_A),
        vec![json!({ "properties": { "title": { "title": title_spans("Done") } }, "archived": true })]
    );
    let page = env.mock.page(AUDIT_PAGE_A).unwrap();
    assert_eq!(page["archived"], true);
    assert_eq!(audit_title_of(&page), "Done");

    // (b) icon + properties on a database row.
    env.mock.clear_requests();
    let notes = json!({ "Notes": { "rich_text": [{ "type": "text", "text": { "content": "shipped" } }] } });
    let resp = patch(AUDIT_PAGE_B, json!({ "icon": "🚀", "properties": notes })).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        notion_patches(AUDIT_PAGE_B),
        vec![json!({ "properties": notes, "icon": { "type": "emoji", "emoji": "🚀" } })]
    );
    let row = env.mock.page(AUDIT_PAGE_B).unwrap();
    assert_eq!(row["icon"]["emoji"], "🚀");
    assert_eq!(audit_joined(&row["properties"]["Notes"]["rich_text"]), "shipped");

    // (c) A single field sends exactly what it sent before.
    env.mock.clear_requests();
    assert_eq!(patch(AUDIT_PAGE_A, json!({ "archived": false })).await.unwrap().status(), StatusCode::OK);
    assert_eq!(patch(AUDIT_PAGE_A, json!({ "title": "x" })).await.unwrap().status(), StatusCode::OK);
    assert_eq!(patch(AUDIT_PAGE_A, json!({ "icon": "📌" })).await.unwrap().status(), StatusCode::OK);
    assert_eq!(
        notion_patches(AUDIT_PAGE_A),
        vec![
            json!({ "archived": false }),
            json!({ "properties": { "title": { "title": title_spans("x") } } }),
            json!({ "icon": { "type": "emoji", "emoji": "📌" } }),
        ]
    );

    // (d) Nothing to change is a client error and never reaches Notion.
    env.mock.clear_requests();
    for body in [json!({}), json!({ "icon": "  " })] {
        let resp = patch(AUDIT_PAGE_A, body.clone()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{body}");
        let err: Value = resp.json().await.unwrap();
        assert!(err["error"].as_str().unwrap().contains("At least one of title, icon, archived, properties"), "{err}");
    }
    assert!(notion_patches(AUDIT_PAGE_A).is_empty());
    api_server.abort();
    env.server.abort();
}

fn audit_block_texts(env: &AuditEnv, parent: &str) -> Vec<String> {
    env.mock
        .children_of(parent)
        .iter()
        .map(|b| audit_joined(&b[b["type"].as_str().unwrap()]["rich_text"]))
        .collect()
}

#[tokio::test]
async fn regression_n13_stale_retry_refuses_to_overwrite_remote_edit() {
    let env = start_audit_env().await;
    let workspace = json!({ "type": "workspace", "workspace": true });
    env.mock.add_page(audit_page(AUDIT_PAGE_A, "Notes", workspace, "2026-11-20T02:00:00.000Z"));
    let b1 = "b1000000-0000-4000-8000-000000000001";
    let b2 = "b1000000-0000-4000-8000-000000000002";
    let reset = || {
        env.mock.set_children(
            AUDIT_PAGE_A,
            vec![audit_block(b1, "paragraph", "Alpha"), audit_block(b2, "paragraph", "Beta")],
        );
    };
    let edited = "Alpha edited\n\nBeta";

    // A teammate deleted block 1 and rewrote block 2 after our cache was taken.
    reset();
    let cached = sync::sync_page_detail_force(&env.client, &env.storage, AUDIT_PAGE_A).await.unwrap();
    env.mock.set_children(AUDIT_PAGE_A, vec![audit_block(b2, "paragraph", "Beta, rewritten by a teammate")]);
    let err = notion_cli::cli::op_diff_sync_page_markdown(
        &env.client,
        &env.storage,
        AUDIT_PAGE_A,
        edited,
        Some(cached.blocks),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("Reload the page before saving"), "{err}");
    assert_eq!(audit_block_texts(&env, AUDIT_PAGE_A), vec!["Beta, rewritten by a teammate"]);

    // Only the deletion: the retry the path was built for still succeeds.
    reset();
    let cached = sync::sync_page_detail_force(&env.client, &env.storage, AUDIT_PAGE_A).await.unwrap();
    env.mock.set_children(AUDIT_PAGE_A, vec![audit_block(b2, "paragraph", "Beta")]);
    let detail = notion_cli::cli::op_diff_sync_page_markdown(
        &env.client,
        &env.storage,
        AUDIT_PAGE_A,
        edited,
        Some(cached.blocks),
    )
    .await
    .unwrap();
    assert_eq!(audit_block_texts(&env, AUDIT_PAGE_A), vec!["Alpha edited", "Beta"]);
    assert_eq!(detail.blocks.len(), 2);
    env.server.abort();
}

#[tokio::test]
async fn regression_n12_editor_inserts_real_table() {
    let env = start_audit_env().await;
    let workspace = json!({ "type": "workspace", "workspace": true });
    env.mock.add_page(audit_page(AUDIT_PAGE_A, "Notes", workspace, "2026-11-20T02:00:00.000Z"));
    env.mock.set_children(AUDIT_PAGE_A, vec![audit_block("b1200000-0000-4000-8000-000000000001", "paragraph", "Intro")]);

    // A table typed into the editor becomes one table block.
    let edited = "Intro\n\n| Name | Score |\n| --- | --- |\n| Ann | 3 |\n| Bo | 5 |";
    notion_cli::cli::op_diff_sync_page_markdown(&env.client, &env.storage, AUDIT_PAGE_A, edited, None)
        .await
        .unwrap();
    let appends = env.mock.requests_matching("PATCH", &format!("/v1/blocks/{AUDIT_PAGE_A}/children"));
    assert_eq!(appends.len(), 1, "{appends:?}");
    let sent = appends[0].body["children"].as_array().unwrap();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0]["type"], "table");
    let rows = sent[0]["table"]["children"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|r| r["type"] == "table_row"));
    let pipe_paragraphs = env
        .mock
        .children_of(AUDIT_PAGE_A)
        .iter()
        .filter(|b| b["type"] == "paragraph" && audit_joined(&b["paragraph"]["rich_text"]).starts_with('|'))
        .count();
    assert_eq!(pipe_paragraphs, 0);

    // 150 rows: 100 go with the table, the other 50 are appended to it. The mock rejects
    // more than 100 children in one request, as Notion does.
    env.mock.clear_requests();
    let mut big = String::from("| N | Square |\n| --- | --- |");
    for n in 1..150 {
        big.push_str(&format!("\n| {n} | {} |", n * n));
    }
    let created = notion_cli::cli::op_append_blocks(&env.client, &env.storage, AUDIT_PAGE_A, &big)
        .await
        .unwrap();
    assert_eq!(created.len(), 1);
    let table_id = created[0].id.clone();
    let appends = env.mock.requests_matching("PATCH", "/v1/blocks/");
    let appends: Vec<_> = appends.into_iter().filter(|r| r.path.ends_with("/children")).collect();
    assert_eq!(appends.len(), 2, "{:?}", appends.iter().map(|r| &r.path).collect::<Vec<_>>());
    assert_eq!(appends[0].path, format!("/v1/blocks/{AUDIT_PAGE_A}/children"));
    assert_eq!(appends[0].body["children"][0]["table"]["children"].as_array().unwrap().len(), 100);
    assert_eq!(appends[1].path, format!("/v1/blocks/{table_id}/children"));
    assert_eq!(appends[1].body["children"].as_array().unwrap().len(), 50);
    let stored = env.mock.children_of(&table_id);
    assert_eq!(stored.len(), 150);
    assert_eq!(audit_joined(&stored[149]["table_row"]["cells"][0]), "149");
    env.server.abort();
}

#[tokio::test]
async fn regression_n19_download_keeps_korean_filename() {
    let env = start_audit_env().await;
    let workspace = json!({ "type": "workspace", "workspace": true });
    env.mock.add_page(audit_page(AUDIT_PAGE_A, "Minutes", workspace, "2026-11-20T02:00:00.000Z"));
    env.mock.media.lock().unwrap().insert("minutes.pdf".into(), b"%PDF-1.4 minutes".to_vec());
    let base = env.cfg.api_base_url.trim_end_matches("/v1").to_string();
    let file_block = "f1900000-0000-4000-8000-000000000001";
    env.mock.set_children(
        AUDIT_PAGE_A,
        vec![json!({
            "object": "block",
            "id": file_block,
            "type": "file",
            "has_children": false,
            "file": {
                "type": "external",
                "external": { "url": format!("{base}/media/minutes.pdf") },
                "name": "2026년_회의록.pdf",
                "caption": []
            }
        })],
    );
    let dir = tempfile::tempdir().unwrap();
    let dl = media::download_media_target(&env.cfg, &env.client, file_block, Some(dir.path()))
        .await
        .unwrap();
    assert_eq!(dl.file_name, "2026년_회의록.pdf");
    let path = dir.path().join("2026년_회의록.pdf");
    assert!(path.exists(), "{}", dl.path);
    assert_eq!(std::fs::read(&path).unwrap(), b"%PDF-1.4 minutes");
    env.server.abort();
}
