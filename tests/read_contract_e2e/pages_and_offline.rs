use std::{
    collections::{BTreeSet, HashSet},
    sync::{atomic::Ordering, Arc},
};

use axum::http::StatusCode;
use notion_cli::{mcp, serve};
use serde_json::{json, Value};
use tokio::net::TcpListener;

use crate::harness::{
    mcp_runtime, notion_cli_cmd, page_obj, paragraph_block, standard_mock, start_mock_server,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_pages_get_json_defaults_to_compact_and_full_restores_detail() {
    let mock = standard_mock();
    let (base_url, server) = start_mock_server(mock).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");

    // Default --json is compact
    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-roadmap", "--json"])
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let compact_bytes = out.stdout.len();
    let compact: Value = serde_json::from_slice(&out.stdout).unwrap();
    let compact_keys: BTreeSet<&str> = compact
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        compact_keys,
        BTreeSet::from([
            "comments_state",
            "hint",
            "id",
            "incomplete_blocks",
            "last_edited_time",
            "markdown",
            "omitted",
            "title",
        ])
    );
    assert!(compact.get("comments").is_none());
    assert!(compact.get("comments_has_more").is_none());
    assert_eq!(
        compact["hint"],
        "notion pages get page-roadmap --json --full"
    );

    // --json --full restores full detail + incomplete_blocks + comments_has_more
    let out_full = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-roadmap", "--json", "--full"])
        .output()
        .await
        .unwrap();
    assert!(
        out_full.status.success(),
        "{}",
        String::from_utf8_lossy(&out_full.stderr)
    );
    let full_bytes = out_full.stdout.len();
    let full: Value = serde_json::from_slice(&out_full.stdout).unwrap();
    assert!(full.get("page").is_some());
    assert!(full.get("blocks").is_some());
    assert_eq!(full["comments"].as_array().unwrap().len(), 2);
    assert_eq!(full["incomplete_blocks"], json!([]));
    assert_eq!(full["comments_has_more"], json!(false));

    eprintln!(
        "MEASURE_CLI pages_get_json_compact_bytes={} pages_get_json_full_bytes={}",
        compact_bytes, full_bytes
    );

    // --json --compact alias matches compact key set
    let out_alias = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-roadmap", "--json", "--compact"])
        .output()
        .await
        .unwrap();
    assert!(out_alias.status.success());
    let alias: Value = serde_json::from_slice(&out_alias.stdout).unwrap();
    let alias_keys: BTreeSet<&str> = alias
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(alias_keys, compact_keys);

    // Negative controls: exit 2
    let conflict = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "pages",
            "get",
            "page-roadmap",
            "--json",
            "--compact",
            "--full",
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(conflict.status.code(), Some(2));

    let no_json = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-roadmap", "--full"])
        .output()
        .await
        .unwrap();
    assert_eq!(no_json.status.code(), Some(2));

    // Positive controls: text pages get and pages export unchanged
    let text_out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-roadmap"])
        .output()
        .await
        .unwrap();
    assert!(text_out.status.success());
    assert!(String::from_utf8_lossy(&text_out.stdout).contains("## Goals"));

    let export_out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "export", "page-roadmap"])
        .output()
        .await
        .unwrap();
    assert!(export_out.status.success());
    assert!(String::from_utf8_lossy(&export_out.stdout).starts_with("# Roadmap\n\n"));

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compact_read_issues_no_comment_requests_and_keeps_comment_cache() {
    let mock = standard_mock();
    let (base_url, server) = start_mock_server(mock.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");

    // 1. Cold cache compact read
    let out1 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-roadmap", "--json"])
        .output()
        .await
        .unwrap();
    assert!(
        out1.status.success(),
        "{}",
        String::from_utf8_lossy(&out1.stderr)
    );
    let v1: Value = serde_json::from_slice(&out1.stdout).unwrap();
    assert_eq!(mock.comment_reqs.load(Ordering::SeqCst), 0);
    assert_eq!(v1["comments_state"], "not_fetched");
    assert!(v1["omitted"]["comments"].is_null());
    assert!(v1["markdown"].as_str().unwrap().contains("Ship v1.0"));

    // 2. Warm compact read reuses body marker (0 new block GETs, 0 comment GETs)
    let blocks_before = mock.block_reqs.load(Ordering::SeqCst);
    let out2 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-roadmap", "--json"])
        .output()
        .await
        .unwrap();
    assert!(out2.status.success());
    assert_eq!(mock.comment_reqs.load(Ordering::SeqCst), 0);
    assert_eq!(mock.block_reqs.load(Ordering::SeqCst), blocks_before);

    // 3. Full read fetches comments (1 comment request)
    let out3 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-roadmap", "--json", "--full"])
        .output()
        .await
        .unwrap();
    assert!(out3.status.success());
    assert_eq!(mock.comment_reqs.load(Ordering::SeqCst), 1);

    // 4. Compact read now sees cached count 2 without requesting comments
    let out4 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-roadmap", "--json"])
        .output()
        .await
        .unwrap();
    assert!(out4.status.success());
    let v4: Value = serde_json::from_slice(&out4.stdout).unwrap();
    assert_eq!(mock.comment_reqs.load(Ordering::SeqCst), 1);
    assert_eq!(v4["comments_state"], "cached");
    assert_eq!(v4["omitted"]["comments"], 2);

    // 5. Mutate last_edited_time and blocks; compact refetches blocks only, keeps cached comment state
    {
        mock.pages.lock().unwrap().insert(
            "page-roadmap".into(),
            page_obj("page-roadmap", "Roadmap", "2026-11-02T00:00:00.000Z"),
        );
        mock.blocks.lock().unwrap().insert(
            "page-roadmap".into(),
            vec![paragraph_block("blk-rm-new", "Ship v2.0")],
        );
    }
    let out5 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-roadmap", "--json"])
        .output()
        .await
        .unwrap();
    assert!(out5.status.success());
    let v5: Value = serde_json::from_slice(&out5.stdout).unwrap();
    assert_eq!(mock.comment_reqs.load(Ordering::SeqCst), 1);
    assert!(v5["markdown"].as_str().unwrap().contains("Ship v2.0"));
    assert_eq!(v5["comments_state"], "cached");
    assert_eq!(v5["omitted"]["comments"], 2);

    // 6. Offline comments list still has the 2 cached comments
    let out6 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["comments", "list", "page-roadmap", "--offline", "--json"])
        .output()
        .await
        .unwrap();
    assert!(
        out6.status.success(),
        "{}",
        String::from_utf8_lossy(&out6.stderr)
    );
    let v6: Value = serde_json::from_slice(&out6.stdout).unwrap();
    assert_eq!(v6["count"], 2);

    // 7. 403 page: full records comments_error, compact reports failed without requesting comments, offline full keeps it
    let out7a = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-cmt-err", "--json", "--full"])
        .output()
        .await
        .unwrap();
    assert!(out7a.status.success());
    let full_err: Value = serde_json::from_slice(&out7a.stdout).unwrap();
    let expected_err = full_err["comments_error"].as_str().unwrap().to_string();
    assert!(
        expected_err.contains("403"),
        "expected 403 in {expected_err}"
    );

    let comment_reqs_after_full = mock.comment_reqs.load(Ordering::SeqCst);
    let out7b = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-cmt-err", "--json"])
        .output()
        .await
        .unwrap();
    assert!(out7b.status.success());
    assert_eq!(
        mock.comment_reqs.load(Ordering::SeqCst),
        comment_reqs_after_full
    );
    let v7b: Value = serde_json::from_slice(&out7b.stdout).unwrap();
    assert_eq!(v7b["comments_state"], "failed");
    assert_eq!(v7b["comments_error"], expected_err);
    assert!(v7b["omitted"]["comments"].is_null());

    let out7c = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "pages",
            "get",
            "page-cmt-err",
            "--offline",
            "--json",
            "--full",
        ])
        .output()
        .await
        .unwrap();
    assert!(out7c.status.success());
    let v7c: Value = serde_json::from_slice(&out7c.stdout).unwrap();
    assert_eq!(v7c["comments_error"], expected_err);

    // 8. MCP get_page on fresh storage issues 0 comment requests
    let comment_reqs_before_mcp = mock.comment_reqs.load(Ordering::SeqCst);
    let (cfg, auth, client, storage) = mcp_runtime(&base_url);
    let mcp_resp = mcp::handle_request_value(
        &cfg,
        &auth,
        &client,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "get_page", "arguments": { "page": "page-roadmap" } }
        }),
    )
    .await
    .unwrap();
    assert_eq!(mcp_resp["result"]["isError"], false, "{mcp_resp}");
    assert_eq!(
        mock.comment_reqs.load(Ordering::SeqCst),
        comment_reqs_before_mcp
    );
    assert_eq!(
        mcp_resp["result"]["structuredContent"]["comments_state"],
        "not_fetched"
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deep_tree_reads_13_levels_and_flags_the_14th() {
    let mock = standard_mock();
    let (base_url, server) = start_mock_server(mock).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");

    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-deep", "--json", "--full"])
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let deep: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(deep["incomplete_blocks"], json!(["blk-depth-13"]));
    let md = deep["markdown"].as_str().unwrap();
    assert!(md.contains("Level 13"), "level 13 should be loaded: {md}");
    assert!(!md.contains("HIDDEN"), "level 14 must not be loaded: {md}");

    let export_path = tmp.path().join("deep.md");
    let export_out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "pages",
            "export",
            "page-deep",
            "--out",
            export_path.to_str().unwrap(),
        ])
        .output()
        .await
        .unwrap();
    assert!(export_out.status.success());
    let stderr = String::from_utf8_lossy(&export_out.stderr);
    assert!(
        stderr.contains("Warning:"),
        "expected warning on stderr: {stderr}"
    );
    assert!(
        stderr.contains("blk-depth-13"),
        "expected blk-depth-13 on stderr: {stderr}"
    );

    // Direct lookup of blk-depth-13 loads HIDDEN
    let direct = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["blocks", "list", "blk-depth-13", "--json"])
        .output()
        .await
        .unwrap();
    assert!(direct.status.success());
    let direct_val: Value = serde_json::from_slice(&direct.stdout).unwrap();
    assert!(serde_json::to_string(&direct_val["blocks"])
        .unwrap()
        .contains("HIDDEN"));
    assert_eq!(direct_val["incomplete_blocks"], json!([]));

    // Control: 6-level tree loads completely with no warning
    let shallow_out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-shallow6", "--json", "--full"])
        .output()
        .await
        .unwrap();
    assert!(shallow_out.status.success());
    let shallow: Value = serde_json::from_slice(&shallow_out.stdout).unwrap();
    assert_eq!(shallow["incomplete_blocks"], json!([]));
    assert!(shallow["markdown"].as_str().unwrap().contains("LEVEL6_OK"));

    let shallow_export = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "export", "page-shallow6"])
        .output()
        .await
        .unwrap();
    assert!(shallow_export.status.success());
    assert!(
        String::from_utf8_lossy(&shallow_export.stderr)
            .trim()
            .is_empty(),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&shallow_export.stderr)
    );
    assert!(String::from_utf8_lossy(&shallow_export.stdout).contains("LEVEL6_OK"));

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_cold_cache_is_an_error_and_fetched_empty_is_not() {
    let mock = standard_mock();
    let (base_url, server) = start_mock_server(mock).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");

    // 1. Sync only
    let sync_out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["sync", "--json"])
        .output()
        .await
        .unwrap();
    assert!(sync_out.status.success());

    // Unfetched body -> exit 1 (pages get and pages export)
    let cold_get = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-roadmap", "--offline", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(cold_get.status.code(), Some(1));
    let cold_err: Value = serde_json::from_slice(&cold_get.stdout).unwrap();
    assert_eq!(cold_err["ok"], false);
    assert_eq!(cold_err["error"]["code"], "not_cached");
    assert!(String::from_utf8_lossy(&cold_get.stderr).contains(
        "Page 'page-roadmap' body not found in offline cache (not fetched); read it online once"
    ));

    let cold_export = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "export", "page-roadmap", "--offline"])
        .output()
        .await
        .unwrap();
    assert_eq!(cold_export.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&cold_export.stderr).contains(
        "Page 'page-roadmap' body not found in offline cache (not fetched); read it online once"
    ));

    let cold_cmts = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["comments", "list", "page-roadmap", "--offline", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(cold_cmts.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&cold_cmts.stderr)
        .contains("Comments of 'page-roadmap' not found in offline cache (not fetched)"));

    // 2. Compact read of genuinely empty page caches body only
    let empty_compact = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-empty", "--json"])
        .output()
        .await
        .unwrap();
    assert!(empty_compact.status.success());

    let empty_off_compact = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-empty", "--offline", "--json"])
        .output()
        .await
        .unwrap();
    assert!(empty_off_compact.status.success());
    let empty_val: Value = serde_json::from_slice(&empty_off_compact.stdout).unwrap();
    assert_eq!(empty_val["markdown"], "");
    assert_eq!(empty_val["omitted"]["blocks"], 0);
    assert_eq!(empty_val["comments_state"], "not_fetched");
    assert!(empty_val["omitted"]["comments"].is_null());

    // Comments still unfetched -> exit 1 on comments list, and offline full reports comments_error
    let empty_cmts_before_full = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["comments", "list", "page-empty", "--offline", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(empty_cmts_before_full.status.code(), Some(1));

    let empty_off_full_before = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "pages",
            "get",
            "page-empty",
            "--offline",
            "--json",
            "--full",
        ])
        .output()
        .await
        .unwrap();
    assert!(empty_off_full_before.status.success());
    let empty_off_full_v: Value = serde_json::from_slice(&empty_off_full_before.stdout).unwrap();
    assert_eq!(
        empty_off_full_v["comments_error"],
        "Comments not fetched into the offline cache"
    );

    // Full read fetches verified empty comments (0)
    let empty_full = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-empty", "--json", "--full"])
        .output()
        .await
        .unwrap();
    assert!(empty_full.status.success());

    let empty_off_after_full = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-empty", "--offline", "--json"])
        .output()
        .await
        .unwrap();
    assert!(empty_off_after_full.status.success());
    let v_after: Value = serde_json::from_slice(&empty_off_after_full.stdout).unwrap();
    assert_eq!(v_after["comments_state"], "cached");
    assert_eq!(v_after["omitted"]["blocks"], 0);
    assert_eq!(v_after["omitted"]["comments"], 0);

    let empty_cmts_after_full = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["comments", "list", "page-empty", "--offline", "--json"])
        .output()
        .await
        .unwrap();
    assert!(empty_cmts_after_full.status.success());
    let c_after: Value = serde_json::from_slice(&empty_cmts_after_full.stdout).unwrap();
    assert_eq!(c_after["count"], 0);
    assert_eq!(c_after["returned"], 0);
    assert_eq!(c_after["has_more"], false);

    // 3. 403 comments error persists offline
    let err_online = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["comments", "list", "page-cmt-err", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(err_online.status.code(), Some(1));

    let err_full = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-cmt-err", "--json", "--full"])
        .output()
        .await
        .unwrap();
    assert!(err_full.status.success());

    let err_compact = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-cmt-err", "--json"])
        .output()
        .await
        .unwrap();
    assert!(err_compact.status.success());
    let ec: Value = serde_json::from_slice(&err_compact.stdout).unwrap();
    assert_eq!(ec["comments_state"], "failed");
    assert!(ec["comments_error"].as_str().unwrap().contains("403"));
    assert!(ec["omitted"]["comments"].is_null());

    let err_off = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["comments", "list", "page-cmt-err", "--offline", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(err_off.status.code(), Some(1));
    let err_off_stderr = String::from_utf8_lossy(&err_off.stderr);
    assert!(
        err_off_stderr
            .contains("Comments of 'page-cmt-err' not found in offline cache (last fetch failed:")
            && err_off_stderr.contains("403"),
        "unexpected stderr: {err_off_stderr}"
    );

    // HTTP 404 check on cold offline page body & comments
    let (cfg, auth, client, storage) = mcp_runtime(&base_url);
    notion_cli::sync::sync_workspace_tree(&client, &storage)
        .await
        .unwrap();
    let api_state = Arc::new(serve::ApiState {
        config: cfg,
        auth,
        client,
        storage,
        api_key: None,
    });
    let api_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = api_listener.local_addr().unwrap();
    let api_server = tokio::spawn(async move {
        let _ = axum::serve(api_listener, serve::build_router(api_state)).await;
    });
    let http = reqwest::Client::new();
    let r_page = http
        .get(format!(
            "http://{api_addr}/api/v1/pages/page-roadmap?offline=true"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r_page.status(), StatusCode::NOT_FOUND);
    let r_cmts = http
        .get(format!(
            "http://{api_addr}/api/v1/pages/page-roadmap/comments?offline=true"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r_cmts.status(), StatusCode::NOT_FOUND);
    api_server.abort();

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_db_query_rejects_filter_and_sorts() {
    let mock = standard_mock();
    let (base_url, server) = start_mock_server(mock).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");

    // Seed cache online
    let seed = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["db", "query", "db-tasks", "--json"])
        .output()
        .await
        .unwrap();
    assert!(
        seed.status.success(),
        "{}",
        String::from_utf8_lossy(&seed.stderr)
    );
    let seed_val: Value = serde_json::from_slice(&seed.stdout).unwrap();
    assert_eq!(seed_val["count"], 3);

    // Online filter control
    let online_f = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "db",
            "query",
            "db-tasks",
            "--filter",
            r#"{"property":"Status","status":{"equals":"Done"}}"#,
            "--json",
        ])
        .output()
        .await
        .unwrap();
    assert!(online_f.status.success());
    let online_f_val: Value = serde_json::from_slice(&online_f.stdout).unwrap();
    assert_eq!(online_f_val["count"], 1);

    // Offline + filter -> exit 1
    let off_f = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "db",
            "query",
            "db-tasks",
            "--offline",
            "--filter",
            r#"{"property":"Status","status":{"equals":"Done"}}"#,
            "--json",
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(off_f.status.code(), Some(1));
    let off_err: Value = serde_json::from_slice(&off_f.stdout).unwrap();
    assert_eq!(off_err["ok"], false);
    assert_eq!(off_err["error"]["code"], "invalid_input");
    assert!(String::from_utf8_lossy(&off_f.stderr).contains("Invalid offline query"));

    // Offline + sorts -> exit 1
    let off_s = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "db",
            "query",
            "db-tasks",
            "--offline",
            "--sorts",
            r#"[{"property":"Status","direction":"ascending"}]"#,
            "--json",
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(off_s.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&off_s.stderr).contains("Invalid offline query"));

    // Offline without filter/sorts -> exit 0, 3 rows
    let off_ok = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["db", "query", "db-tasks", "--offline", "--json"])
        .output()
        .await
        .unwrap();
    assert!(off_ok.status.success());
    let off_ok_val: Value = serde_json::from_slice(&off_ok.stdout).unwrap();
    assert_eq!(off_ok_val["count"], 3);
    assert_eq!(off_ok_val["returned"], 3);

    // MCP offline + filter -> isError: true
    let (cfg, auth, client, storage) = mcp_runtime(&base_url);
    let mcp_err = mcp::handle_request_value(
        &cfg,
        &auth,
        &client,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "query_database",
                "arguments": {
                    "database": "db-tasks",
                    "offline": true,
                    "filter": { "property": "Status", "status": { "equals": "Done" } }
                }
            }
        }),
    )
    .await
    .unwrap();
    assert_eq!(mcp_err["result"]["isError"], true);
    assert!(mcp_err["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("Invalid offline query"));

    // HTTP POST /api/v1/databases/db-tasks/query with offline:true + filter -> 400
    let api_state = Arc::new(serve::ApiState {
        config: cfg,
        auth,
        client,
        storage,
        api_key: None,
    });
    let api_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = api_listener.local_addr().unwrap();
    let api_server = tokio::spawn(async move {
        let _ = axum::serve(api_listener, serve::build_router(api_state)).await;
    });
    let http = reqwest::Client::new();
    let r_bad = http
        .post(format!("http://{api_addr}/api/v1/databases/db-tasks/query"))
        .json(&json!({
            "offline": true,
            "filter": { "property": "Status", "status": { "equals": "Done" } }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r_bad.status(), StatusCode::BAD_REQUEST);
    api_server.abort();

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn db_query_pages_with_cursor_without_dropping_rows() {
    let mock = standard_mock();
    let (base_url, server) = start_mock_server(mock).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");

    let p1 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["db", "query", "db-tasks", "--limit", "2", "--json"])
        .output()
        .await
        .unwrap();
    assert!(p1.status.success());
    let v1: Value = serde_json::from_slice(&p1.stdout).unwrap();
    assert_eq!(v1["count"], 2);
    assert_eq!(v1["returned"], 2);
    assert_eq!(v1["has_more"], true);
    let cur = v1["next_cursor"].as_str().unwrap().to_string();

    let p2 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "db", "query", "db-tasks", "--limit", "2", "--cursor", &cur, "--json",
        ])
        .output()
        .await
        .unwrap();
    assert!(p2.status.success());
    let v2: Value = serde_json::from_slice(&p2.stdout).unwrap();
    assert_eq!(v2["count"], 1);
    assert_eq!(v2["returned"], 1);
    assert_eq!(v2["has_more"], false);
    assert!(v2["next_cursor"].is_null());

    let mut ids = HashSet::new();
    for row in v1["rows"]
        .as_array()
        .unwrap()
        .iter()
        .chain(v2["rows"].as_array().unwrap())
    {
        ids.insert(row["id"].as_str().unwrap().to_string());
    }
    assert_eq!(ids.len(), 3);

    // Cache offset pagination & invalid cursor / limit 0 negative controls
    let db1 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["db", "list", "--json", "--limit", "1"])
        .output()
        .await
        .unwrap();
    assert!(db1.status.success());
    let db1_v: Value = serde_json::from_slice(&db1.stdout).unwrap();
    assert_eq!(db1_v["has_more"], true);
    assert_eq!(db1_v["next_cursor"], "1");

    let db_bad_cur = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["db", "list", "--json", "--cursor", "abc"])
        .output()
        .await
        .unwrap();
    assert_eq!(db_bad_cur.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&db_bad_cur.stderr)
        .contains("Invalid cursor 'abc': expected the integer offset returned as next_cursor"));

    let bad_lim = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["comments", "list", "page-roadmap", "--json", "--limit", "0"])
        .output()
        .await
        .unwrap();
    assert_eq!(bad_lim.status.code(), Some(1));
    let bad_lim_json: Value = serde_json::from_slice(&bad_lim.stdout).unwrap();
    assert_eq!(bad_lim_json["ok"], false);
    assert_eq!(bad_lim_json["error"]["code"], "invalid_input");
    assert!(
        String::from_utf8_lossy(&bad_lim.stderr).contains("Invalid limit 0: must be at least 1")
    );

    // W3: Remote 404 returns object_not_found JSON envelope on --json, and empty stdout without --json
    let missing_uuid = "99999999-9999-9999-9999-999999999999";
    let not_found_json = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", missing_uuid, "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(not_found_json.status.code(), Some(1));
    let nf_val: Value = serde_json::from_slice(&not_found_json.stdout).unwrap();
    assert_eq!(nf_val["ok"], false);
    assert_eq!(nf_val["error"]["code"], "object_not_found");

    let not_found_text = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", missing_uuid])
        .output()
        .await
        .unwrap();
    assert_eq!(not_found_text.status.code(), Some(1));
    assert!(
        not_found_text.stdout.is_empty(),
        "text-mode failure must keep stdout empty"
    );

    server.abort();
}

