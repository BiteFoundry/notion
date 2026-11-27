use std::{
    collections::{BTreeSet, HashSet},
    sync::atomic::Ordering,
};

use notion_cli::{mcp, storage::Storage};
use serde_json::{json, Value};

use crate::harness::{
    db_obj, mcp_runtime, notion_cli_cmd, page_obj, standard_mock, start_mock_server, CursorFault,
    ReadContractMock,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn comments_past_2000_are_listed_fully_and_detail_flags_more_without_refetch() {
    let mock = standard_mock();
    let (base_url, server) = start_mock_server(mock.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");

    // 1. Cursor chain reads all 2,100 comments in 21 requests
    let mut cursor: Option<String> = None;
    let mut ids = HashSet::new();
    let mut rounds = 0usize;
    loop {
        rounds += 1;
        let mut args = vec![
            "comments".to_string(),
            "list".to_string(),
            "page-manycmt".to_string(),
            "--json".to_string(),
            "--limit".to_string(),
            "100".to_string(),
        ];
        if let Some(ref c) = cursor {
            args.push("--cursor".to_string());
            args.push(c.clone());
        }
        let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
            .args(&args)
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        for c in v["comments"].as_array().unwrap() {
            ids.insert(c["id"].as_str().unwrap().to_string());
        }
        if !v["has_more"].as_bool().unwrap() {
            assert!(v["next_cursor"].is_null());
            break;
        }
        cursor = v["next_cursor"].as_str().map(str::to_string);
    }
    assert_eq!(rounds, 21);
    assert_eq!(ids.len(), 2100);

    // 2. pages get --json --full fetches 2,001 comments in 21 requests (20x100 + 1x1),
    //    stores all 2,001 in cache, and serializes exactly the first 2,000 with comments_has_more:true
    let before_full = mock.comment_reqs.load(Ordering::SeqCst);
    let full_out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-manycmt", "--json", "--full"])
        .output()
        .await
        .unwrap();
    assert!(full_out.status.success());
    let full_val: Value = serde_json::from_slice(&full_out.stdout).unwrap();
    let full_cmts = full_val["comments"].as_array().unwrap();
    assert_eq!(full_cmts.len(), 2000);
    assert_eq!(full_cmts.first().unwrap()["id"], "mc-0");
    assert_eq!(full_cmts.last().unwrap()["id"], "mc-1999");
    assert_eq!(full_val["comments_has_more"], true);
    assert!(full_val.get("comments_error").is_none());
    assert_eq!(mock.comment_reqs.load(Ordering::SeqCst) - before_full, 21);

    // Repeating --json --full before the page changes reuses cached comments (0 comment GETs)
    let before_full_repeat = mock.comment_reqs.load(Ordering::SeqCst);
    let full_repeat = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-manycmt", "--json", "--full"])
        .output()
        .await
        .unwrap();
    assert!(full_repeat.status.success());
    let full_repeat_val: Value = serde_json::from_slice(&full_repeat.stdout).unwrap();
    assert_eq!(full_repeat_val["comments"].as_array().unwrap().len(), 2000);
    assert_eq!(full_repeat_val["comments_has_more"], true);
    assert_eq!(mock.comment_reqs.load(Ordering::SeqCst), before_full_repeat);

    // Offline --json --full on warm partial cache also serializes 2,000 + comments_has_more:true
    let full_off = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "pages",
            "get",
            "page-manycmt",
            "--offline",
            "--json",
            "--full",
        ])
        .output()
        .await
        .unwrap();
    assert!(full_off.status.success());
    let full_off_val: Value = serde_json::from_slice(&full_off.stdout).unwrap();
    assert_eq!(full_off_val["comments"].as_array().unwrap().len(), 2000);
    assert_eq!(full_off_val["comments_has_more"], true);

    // 3. Compact read issues 0 comment requests, reports partial + null omitted.comments
    let before_compact = mock.comment_reqs.load(Ordering::SeqCst);
    let compact_out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-manycmt", "--json"])
        .output()
        .await
        .unwrap();
    assert!(compact_out.status.success());
    assert_eq!(mock.comment_reqs.load(Ordering::SeqCst), before_compact);
    let compact_val: Value = serde_json::from_slice(&compact_out.stdout).unwrap();
    assert_eq!(compact_val["comments_state"], "partial");
    assert!(compact_val["omitted"]["comments"].is_null());

    // 4. Offline comments list at cursor 2000 proves the 2,001st lookahead comment is preserved in cache
    let off_tail = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "comments",
            "list",
            "page-manycmt",
            "--offline",
            "--json",
            "--limit",
            "100",
            "--cursor",
            "2000",
        ])
        .output()
        .await
        .unwrap();
    assert!(off_tail.status.success());
    let tail_val: Value = serde_json::from_slice(&off_tail.stdout).unwrap();
    assert_eq!(tail_val["returned"], 1);
    assert_eq!(tail_val["count"], 1);
    assert_eq!(tail_val["comments"][0]["id"], "mc-2000");
    assert_eq!(tail_val["has_more"], true);
    assert!(tail_val["next_cursor"].is_null());
    assert!(tail_val["hint"]
        .as_str()
        .unwrap()
        .contains("Only part of the comments is cached"));

    // 5a. Boundary: exactly 2,000 comments (cold + warm + offline + compact)
    let before_2k = mock.comment_reqs.load(Ordering::SeqCst);
    let b2k = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-2000", "--json", "--full"])
        .output()
        .await
        .unwrap();
    assert!(b2k.status.success());
    let b2k_val: Value = serde_json::from_slice(&b2k.stdout).unwrap();
    assert_eq!(b2k_val["comments"].as_array().unwrap().len(), 2000);
    assert_eq!(b2k_val["comments_has_more"], false);
    assert_eq!(mock.comment_reqs.load(Ordering::SeqCst) - before_2k, 20);

    let before_2k_warm = mock.comment_reqs.load(Ordering::SeqCst);
    let b2k_warm = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-2000", "--json", "--full"])
        .output()
        .await
        .unwrap();
    assert!(b2k_warm.status.success());
    let b2k_warm_val: Value = serde_json::from_slice(&b2k_warm.stdout).unwrap();
    assert_eq!(b2k_warm_val["comments"].as_array().unwrap().len(), 2000);
    assert_eq!(b2k_warm_val["comments_has_more"], false);
    assert_eq!(mock.comment_reqs.load(Ordering::SeqCst), before_2k_warm);

    let b2k_compact = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-2000", "--json"])
        .output()
        .await
        .unwrap();
    assert!(b2k_compact.status.success());
    assert_eq!(mock.comment_reqs.load(Ordering::SeqCst), before_2k_warm);
    let b2k_cval: Value = serde_json::from_slice(&b2k_compact.stdout).unwrap();
    assert_eq!(b2k_cval["comments_state"], "cached");
    assert_eq!(b2k_cval["omitted"]["comments"], 2000);

    // 5b. Boundary: exactly 2,001 comments (CLI cold, warm, offline, compact, cached lookahead, online paging)
    let before_2001 = mock.comment_reqs.load(Ordering::SeqCst);
    let b2001 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-2001", "--json", "--full"])
        .output()
        .await
        .unwrap();
    assert!(b2001.status.success());
    let b2001_val: Value = serde_json::from_slice(&b2001.stdout).unwrap();
    let b2001_cmts = b2001_val["comments"].as_array().unwrap();
    assert_eq!(b2001_cmts.len(), 2000);
    assert_eq!(b2001_cmts.last().unwrap()["id"], "mc-1999");
    assert_eq!(b2001_val["comments_has_more"], true);
    assert_eq!(mock.comment_reqs.load(Ordering::SeqCst) - before_2001, 21);

    let before_2001_warm = mock.comment_reqs.load(Ordering::SeqCst);
    let b2001_warm = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-2001", "--json", "--full"])
        .output()
        .await
        .unwrap();
    assert!(b2001_warm.status.success());
    let b2001_warm_val: Value = serde_json::from_slice(&b2001_warm.stdout).unwrap();
    assert_eq!(b2001_warm_val["comments"].as_array().unwrap().len(), 2000);
    assert_eq!(b2001_warm_val["comments_has_more"], true);
    assert_eq!(mock.comment_reqs.load(Ordering::SeqCst), before_2001_warm);

    let b2001_off_tail = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "comments",
            "list",
            "page-2001",
            "--offline",
            "--json",
            "--limit",
            "100",
            "--cursor",
            "2000",
        ])
        .output()
        .await
        .unwrap();
    assert!(b2001_off_tail.status.success());
    let b2001_tail_val: Value = serde_json::from_slice(&b2001_off_tail.stdout).unwrap();
    assert_eq!(b2001_tail_val["returned"], 1);
    assert_eq!(b2001_tail_val["comments"][0]["id"], "mc-2000");
    assert_eq!(b2001_tail_val["has_more"], true);
    assert!(b2001_tail_val["next_cursor"].is_null());

    // 6. Full one-shot list_comments (--limit 100000) upgrades cache to complete (2,100),
    //    while offline full page detail still caps serialized comments at 2,000 + comments_has_more:true
    let all_online = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "comments",
            "list",
            "page-manycmt",
            "--json",
            "--limit",
            "100000",
        ])
        .output()
        .await
        .unwrap();
    assert!(all_online.status.success());
    let all_offline = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "comments",
            "list",
            "page-manycmt",
            "--offline",
            "--json",
            "--limit",
            "100000",
        ])
        .output()
        .await
        .unwrap();
    assert!(all_offline.status.success());
    let all_off_val: Value = serde_json::from_slice(&all_offline.stdout).unwrap();
    assert_eq!(all_off_val["count"], 2100);
    assert_eq!(all_off_val["has_more"], false);
    assert!(all_off_val.get("hint").is_none());

    let full_after_complete = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "pages",
            "get",
            "page-manycmt",
            "--offline",
            "--json",
            "--full",
        ])
        .output()
        .await
        .unwrap();
    assert!(full_after_complete.status.success());
    let full_after_val: Value = serde_json::from_slice(&full_after_complete.stdout).unwrap();
    assert_eq!(full_after_val["comments"].as_array().unwrap().len(), 2000);
    assert_eq!(full_after_val["comments_has_more"], true);

    // 7. MCP cold + warm + offline coverage for 2,000, 2,001, and 2,100 comments
    let (cfg, auth, client, storage) = mcp_runtime(&base_url);
    for (page_id, expected_reqs, expected_has_more) in [
        ("page-2000", 20usize, false),
        ("page-2001", 21usize, true),
        ("page-manycmt", 21usize, true),
    ] {
        let reqs_before = mock.comment_reqs.load(Ordering::SeqCst);
        let cold_mcp = mcp::handle_request_value(
            &cfg,
            &auth,
            &client,
            &storage,
            json!({
                "jsonrpc": "2.0",
                "id": 10,
                "method": "tools/call",
                "params": { "name": "get_page", "arguments": { "page": page_id, "compact": false } }
            }),
        )
        .await
        .unwrap();
        assert_eq!(cold_mcp["result"]["isError"], false, "{cold_mcp}");
        assert_eq!(
            mock.comment_reqs.load(Ordering::SeqCst) - reqs_before,
            expected_reqs
        );
        let sc = &cold_mcp["result"]["structuredContent"];
        let text = cold_mcp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(!text.contains('\n'));
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(&parsed, sc);
        let cmts = sc["comments"].as_array().unwrap();
        assert_eq!(cmts.len(), 2000);
        assert_eq!(cmts.last().unwrap()["id"], "mc-1999");
        assert_eq!(sc["comments_has_more"], expected_has_more);

        // Warm repeat issues 0 comment requests
        let reqs_warm = mock.comment_reqs.load(Ordering::SeqCst);
        let warm_mcp = mcp::handle_request_value(
            &cfg,
            &auth,
            &client,
            &storage,
            json!({
                "jsonrpc": "2.0",
                "id": 11,
                "method": "tools/call",
                "params": { "name": "get_page", "arguments": { "page": page_id, "compact": false } }
            }),
        )
        .await
        .unwrap();
        assert_eq!(warm_mcp["result"]["isError"], false, "{warm_mcp}");
        assert_eq!(mock.comment_reqs.load(Ordering::SeqCst), reqs_warm);
        assert_eq!(
            warm_mcp["result"]["structuredContent"]["comments"]
                .as_array()
                .unwrap()
                .len(),
            2000
        );
        assert_eq!(
            warm_mcp["result"]["structuredContent"]["comments_has_more"],
            expected_has_more
        );

        // Offline MCP get_page matches
        let off_mcp = mcp::handle_request_value(
            &cfg,
            &auth,
            &client,
            &storage,
            json!({
                "jsonrpc": "2.0",
                "id": 12,
                "method": "tools/call",
                "params": { "name": "get_page", "arguments": { "page": page_id, "compact": false, "offline": true } }
            }),
        )
        .await
        .unwrap();
        assert_eq!(off_mcp["result"]["isError"], false, "{off_mcp}");
        assert_eq!(
            off_mcp["result"]["structuredContent"]["comments"]
                .as_array()
                .unwrap()
                .len(),
            2000
        );
        assert_eq!(
            off_mcp["result"]["structuredContent"]["comments_has_more"],
            expected_has_more
        );

        // Partial caches retain the 2,001st lookahead entry for offline list_comments
        if expected_has_more {
            let tail_mcp = mcp::handle_request_value(
                &cfg,
                &auth,
                &client,
                &storage,
                json!({
                    "jsonrpc": "2.0",
                    "id": 13,
                    "method": "tools/call",
                    "params": {
                        "name": "list_comments",
                        "arguments": { "page": page_id, "offline": true, "limit": 100, "cursor": "2000" }
                    }
                }),
            )
            .await
            .unwrap();
            assert_eq!(tail_mcp["result"]["isError"], false, "{tail_mcp}");
            let tsc = &tail_mcp["result"]["structuredContent"];
            assert_eq!(tsc["returned"], 1);
            assert_eq!(tsc["comments"][0]["id"], "mc-2000");
            assert_eq!(tsc["has_more"], true);
            assert!(tsc["next_cursor"].is_null());
        }
    }

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocks_list_json_defaults_to_50_and_text_prints_all() {
    let mock = standard_mock();
    let (base_url, server) = start_mock_server(mock).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");

    // 1. Default --json returns 50, then 50, then 20
    let p1 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["blocks", "list", "page-wide", "--json"])
        .output()
        .await
        .unwrap();
    assert!(p1.status.success());
    let v1: Value = serde_json::from_slice(&p1.stdout).unwrap();
    assert_eq!(v1["count"], 50);
    assert_eq!(v1["returned"], 50);
    assert_eq!(v1["has_more"], true);
    let c1 = v1["next_cursor"].as_str().unwrap().to_string();

    // Partial read did NOT write cache -> offline fails
    let off_partial = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["blocks", "list", "page-wide", "--offline", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(off_partial.status.code(), Some(1));

    let p2 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["blocks", "list", "page-wide", "--json", "--cursor", &c1])
        .output()
        .await
        .unwrap();
    assert!(p2.status.success());
    let v2: Value = serde_json::from_slice(&p2.stdout).unwrap();
    assert_eq!(v2["count"], 50);
    assert_eq!(v2["has_more"], true);
    let c2 = v2["next_cursor"].as_str().unwrap().to_string();

    let p3 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["blocks", "list", "page-wide", "--json", "--cursor", &c2])
        .output()
        .await
        .unwrap();
    assert!(p3.status.success());
    let v3: Value = serde_json::from_slice(&p3.stdout).unwrap();
    assert_eq!(v3["count"], 20);
    assert_eq!(v3["has_more"], false);
    assert!(v3["next_cursor"].is_null());

    let mut ids = HashSet::new();
    for v in [&v1, &v2, &v3] {
        for b in v["blocks"].as_array().unwrap() {
            ids.insert(b["id"].as_str().unwrap().to_string());
        }
    }
    assert_eq!(ids.len(), 120);

    // Full read (--limit 1000) writes cache -> offline succeeds
    let full = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["blocks", "list", "page-wide", "--json", "--limit", "1000"])
        .output()
        .await
        .unwrap();
    assert!(full.status.success());
    let off_full = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["blocks", "list", "page-wide", "--offline", "--json"])
        .output()
        .await
        .unwrap();
    assert!(off_full.status.success());
    let off_val: Value = serde_json::from_slice(&off_full.stdout).unwrap();
    assert_eq!(off_val["count"], 50);
    assert_eq!(off_val["has_more"], true);

    // Text mode prints all 120 blocks
    let text_out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["blocks", "list", "page-wide"])
        .output()
        .await
        .unwrap();
    assert!(text_out.status.success());
    let stdout = String::from_utf8(text_out.stdout).unwrap();
    assert!(stdout.contains("Wide block 0"));
    assert!(stdout.contains("Wide block 119"));

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_streams_past_500_and_upgrades_unmarked_caches_once() {
    let mock = ReadContractMock::default();
    {
        let mut items = mock.search_items.lock().unwrap();
        // 628 pages + 1 db = 629 objects sorted newest-first
        for i in (0..628).rev() {
            let mins = i / 60;
            let secs = i % 60;
            items.push(page_obj(
                &format!("p-{i:04}"),
                &format!("Page {i}"),
                &format!("2026-11-01T00:{mins:02}:{secs:02}.000Z"),
            ));
        }
        items.push(db_obj("db-1", "DB 1", "2026-10-31T23:59:59.000Z"));
    }
    let (base_url, server) = start_mock_server(mock.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");

    // 1st sync: streams all 629 objects across 7 requests
    let s1 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["sync", "--json"])
        .output()
        .await
        .unwrap();
    assert!(
        s1.status.success(),
        "{}",
        String::from_utf8_lossy(&s1.stderr)
    );
    let v1: Value = serde_json::from_slice(&s1.stdout).unwrap();
    assert_eq!(v1["fetched_count"], 629);
    assert_eq!(v1["pages_count"], 628);
    assert_eq!(v1["databases_count"], 1);
    assert_eq!(v1["complete"], true);
    assert_eq!(mock.search_reqs.load(Ordering::SeqCst), 7);

    // 2nd sync: stops at watermark after 1 request
    let before_s2 = mock.search_reqs.load(Ordering::SeqCst);
    let s2 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["sync", "--json"])
        .output()
        .await
        .unwrap();
    assert!(s2.status.success());
    let v2: Value = serde_json::from_slice(&s2.stdout).unwrap();
    assert_eq!(v2["complete"], true);
    assert_eq!(mock.search_reqs.load(Ordering::SeqCst) - before_s2, 1);

    // Separate profile simulating 0.2.2 cache: has watermark but no workspace-complete marker
    let ws_id = v1["workspace_id"].as_str().unwrap().to_string();
    let tmp2 = tempfile::tempdir().unwrap();
    let cfg_dir2 = tmp2.path().join("cfg");
    let data_dir2 = tmp2.path().join("data");
    let db_path = data_dir2.join(&ws_id).join("cache.db");
    {
        let st = Storage::open(&db_path).unwrap();
        st.set_sync_cursor(
            &format!("workspace:{ws_id}"),
            Some("2026-11-01T00:10:27.000Z"),
        )
        .unwrap();
    }
    let before_upg = mock.search_reqs.load(Ordering::SeqCst);
    let upg = notion_cli_cmd(&base_url, &cfg_dir2, &data_dir2)
        .args(["sync", "--json"])
        .output()
        .await
        .unwrap();
    assert!(
        upg.status.success(),
        "{}",
        String::from_utf8_lossy(&upg.stderr)
    );
    let upg_v: Value = serde_json::from_slice(&upg.stdout).unwrap();
    assert_eq!(upg_v["fetched_count"], 629);
    assert_eq!(mock.search_reqs.load(Ordering::SeqCst) - before_upg, 7);

    let st2 = Storage::open(&db_path).unwrap();
    assert_eq!(
        st2.get_sync_cursor(&format!("workspace-complete:{ws_id}"))
            .as_deref(),
        Some("1")
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_failure_keeps_batches_and_does_not_advance_watermark() {
    let mock = ReadContractMock::default();
    {
        let mut items = mock.search_items.lock().unwrap();
        for i in (0..250).rev() {
            let mins = i / 60;
            let secs = i % 60;
            items.push(page_obj(
                &format!("p-{i:04}"),
                &format!("Page {i}"),
                &format!("2026-11-01T00:{mins:02}:{secs:02}.000Z"),
            ));
        }
    }
    let (base_url, server) = start_mock_server(mock.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");
    let db_path = data_dir.join("default").join("cache.db");

    // Scenario 1: Initial sync fails on 2nd search request
    mock.fail_search_on_req.store(2, Ordering::SeqCst);
    let fail1 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["sync", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(fail1.status.code(), Some(1));
    let fail1_err: Value = serde_json::from_slice(&fail1.stdout).unwrap();
    assert_eq!(fail1_err["ok"], false);
    assert_eq!(fail1_err["error"]["code"], "validation_error");
    assert!(
        String::from_utf8_lossy(&fail1.stderr).contains("Workspace sync stopped after 100 objects"),
        "stderr: {}",
        String::from_utf8_lossy(&fail1.stderr)
    );

    // Already-received 100 items stay cached
    let off1 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "list", "--offline", "--json", "--limit", "1000"])
        .output()
        .await
        .unwrap();
    assert!(off1.status.success());
    let off1_v: Value = serde_json::from_slice(&off1.stdout).unwrap();
    assert_eq!(off1_v["count"], 100);

    // Watermark and complete marker untouched (both absent)
    {
        let st = Storage::open(&db_path).unwrap();
        assert_eq!(st.get_sync_cursor("workspace:default"), None);
        assert_eq!(st.get_sync_cursor("workspace-complete:default"), None);
    }

    // Retry succeeds and caches all 250
    let ok1 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["sync", "--json"])
        .output()
        .await
        .unwrap();
    assert!(
        ok1.status.success(),
        "{}",
        String::from_utf8_lossy(&ok1.stderr)
    );
    let ok1_v: Value = serde_json::from_slice(&ok1.stdout).unwrap();
    assert_eq!(ok1_v["pages_count"], 250);
    assert_eq!(ok1_v["complete"], true);

    let ws_id = ok1_v["workspace_id"].as_str().unwrap().to_string();
    let ws_db_path = data_dir.join(&ws_id).join("cache.db");
    let old_wm = Storage::open(&ws_db_path)
        .unwrap()
        .get_sync_cursor(&format!("workspace:{ws_id}"))
        .unwrap();

    // Scenario 2: Add 150 newer items; fail 2nd request of incremental sync
    {
        let mut items = mock.search_items.lock().unwrap();
        let mut newer = Vec::new();
        for i in (0..150).rev() {
            let mins = i / 60;
            let secs = i % 60;
            newer.push(page_obj(
                &format!("p-new-{i:04}"),
                &format!("New Page {i}"),
                &format!("2026-11-02T00:{mins:02}:{secs:02}.000Z"),
            ));
        }
        newer.extend(items.drain(..));
        *items = newer;
    }
    let next_req = mock.search_reqs.load(Ordering::SeqCst) + 2;
    mock.fail_search_on_req.store(next_req, Ordering::SeqCst);

    let fail2 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["sync", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(fail2.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&fail2.stderr).contains("Workspace sync stopped after 100 objects")
    );

    // Watermark must still be old_wm
    assert_eq!(
        Storage::open(&ws_db_path)
            .unwrap()
            .get_sync_cursor(&format!("workspace:{ws_id}"))
            .as_deref(),
        Some(old_wm.as_str())
    );

    // Retry incremental sync completes all 400 pages and advances watermark
    let ok2 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["sync", "--json"])
        .output()
        .await
        .unwrap();
    assert!(
        ok2.status.success(),
        "{}",
        String::from_utf8_lossy(&ok2.stderr)
    );
    let ok2_v: Value = serde_json::from_slice(&ok2.stdout).unwrap();
    assert_eq!(ok2_v["pages_count"], 400);
    assert_eq!(ok2_v["complete"], true);
    let new_wm = Storage::open(&ws_db_path)
        .unwrap()
        .get_sync_cursor(&format!("workspace:{ws_id}"))
        .unwrap();
    assert!(
        new_wm > old_wm,
        "expected watermark to advance: {old_wm} -> {new_wm}"
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stream_workspace_search_rejects_missing_or_repeated_cursor() {
    let mock = ReadContractMock::default();
    mock.search_items
        .lock()
        .unwrap()
        .push(page_obj("p-1", "One", "2026-11-01T00:00:00.000Z"));
    let (base_url, server) = start_mock_server(mock.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");

    *mock.search_cursor_fault.lock().unwrap() = CursorFault::MissingNextCursor;
    let out_missing = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["sync", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(out_missing.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out_missing.stderr).contains("missing its next cursor"),
        "stderr: {}",
        String::from_utf8_lossy(&out_missing.stderr)
    );

    *mock.search_cursor_fault.lock().unwrap() = CursorFault::RepeatedCursor;
    let out_repeat = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["sync", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(out_repeat.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out_repeat.stderr).contains("repeated its pagination cursor"),
        "stderr: {}",
        String::from_utf8_lossy(&out_repeat.stderr)
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_fallback_keeps_database_hits() {
    let mock = ReadContractMock::default();
    {
        let mut items = mock.search_items.lock().unwrap();
        // Neither title nor description contains the query token "yellow", so local FTS refresh
        // returns 0 hits and falls back to SearchResponse hits.
        items.push(page_obj("p-sun", "Sun Notes", "2026-11-01T00:00:00.000Z"));
        items.push(db_obj(
            "db-lemon",
            "Lemon Tracker",
            "2026-11-01T00:00:00.000Z",
        ));
    }
    let (base_url, server) = start_mock_server(mock).await;
    let tmp1 = tempfile::tempdir().unwrap();
    let tmp2 = tempfile::tempdir().unwrap();

    let fallback = notion_cli_cmd(
        &base_url,
        &tmp1.path().join("cfg"),
        &tmp1.path().join("data"),
    )
    .args(["search", "yellow", "--json"])
    .output()
    .await
    .unwrap();
    assert!(fallback.status.success());
    let fb_v: Value = serde_json::from_slice(&fallback.stdout).unwrap();

    let remote = notion_cli_cmd(
        &base_url,
        &tmp2.path().join("cfg"),
        &tmp2.path().join("data"),
    )
    .args(["search", "yellow", "--remote", "--json"])
    .output()
    .await
    .unwrap();
    assert!(remote.status.success());
    let rm_v: Value = serde_json::from_slice(&remote.stdout).unwrap();

    let extract = |v: &Value| -> BTreeSet<(String, String)> {
        v["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| {
                (
                    h["item_type"].as_str().unwrap().to_string(),
                    h["id"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    };
    let fb_set = extract(&fb_v);
    let rm_set = extract(&rm_v);
    assert_eq!(fb_set, rm_set);
    assert!(fb_set.contains(&("database".into(), "db-lemon".into())));

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_sync_now_reports_fetched_count_and_complete() {
    let mock = ReadContractMock::default();
    {
        let mut items = mock.search_items.lock().unwrap();
        for i in (0..250).rev() {
            items.push(page_obj(
                &format!("p-{i:04}"),
                &format!("Page {i}"),
                "2026-11-01T00:00:00.000Z",
            ));
        }
    }
    let (base_url, server) = start_mock_server(mock).await;
    let (cfg, auth, client, storage) = mcp_runtime(&base_url);

    let resp = mcp::handle_request_value(
        &cfg,
        &auth,
        &client,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "sync_now", "arguments": {} }
        }),
    )
    .await
    .unwrap();
    assert_eq!(resp["result"]["isError"], false, "{resp}");
    let sc = &resp["result"]["structuredContent"];
    assert_eq!(sc["fetched_count"], 250);
    assert_eq!(sc["complete"], true);
    assert!(sc.get("has_more").is_none());

    server.abort();
}

