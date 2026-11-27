use std::sync::atomic::Ordering;

use notion_cli::mcp;
use serde_json::{json, Value};

use crate::harness::{
    db_row_obj, mcp_runtime, notion_cli_cmd, page_obj, paragraph_block, standard_mock,
    start_mock_server, ReadContractMock,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_get_page_defaults_compact_and_text_is_minified() {
    let mock = standard_mock();
    let (base_url, server) = start_mock_server(mock).await;
    let (cfg, auth, client, storage) = mcp_runtime(&base_url);

    // Compact default
    let compact_resp = mcp::handle_request_value(
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
    assert_eq!(compact_resp["result"]["isError"], false, "{compact_resp}");
    let compact_text = compact_resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(!compact_text.contains('\n'), "MCP text must be minified");
    let parsed_compact: Value = serde_json::from_str(compact_text).unwrap();
    assert_eq!(parsed_compact, compact_resp["result"]["structuredContent"]);
    assert_eq!(parsed_compact["comments_state"], "not_fetched");
    assert_eq!(
        parsed_compact["hint"],
        "Call get_page with {\"page\":\"page-roadmap\",\"fields\":[\"<property name>\",...]} to fetch specific properties or fields without pulling full blocks or comments, or {\"page\":\"page-roadmap\",\"compact\":false} for full detail."
    );

    // Full (compact: false)
    let full_resp = mcp::handle_request_value(
        &cfg,
        &auth,
        &client,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "get_page", "arguments": { "page": "page-roadmap", "compact": false } }
        }),
    )
    .await
    .unwrap();
    assert_eq!(full_resp["result"]["isError"], false, "{full_resp}");
    let full_text = full_resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(!full_text.contains('\n'), "MCP text must be minified");
    let parsed_full: Value = serde_json::from_str(full_text).unwrap();
    assert_eq!(parsed_full, full_resp["result"]["structuredContent"]);
    assert!(parsed_full.get("page").is_some());
    assert_eq!(parsed_full["comments_has_more"], false);

    let mcp_compact_wire = serde_json::to_string(&compact_resp).unwrap().len();
    let mcp_full_wire = serde_json::to_string(&full_resp).unwrap().len();
    eprintln!(
        "MEASURE_MCP get_page_compact_wire_bytes={} get_page_full_wire_bytes={}",
        mcp_compact_wire, mcp_full_wire
    );

    // Paginated get_page_blocks
    let blocks_resp = mcp::handle_request_value(
        &cfg,
        &auth,
        &client,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "get_page_blocks", "arguments": { "page": "page-roadmap", "limit": 1 } }
        }),
    )
    .await
    .unwrap();
    assert_eq!(blocks_resp["result"]["isError"], false, "{blocks_resp}");
    let bsc = &blocks_resp["result"]["structuredContent"];
    assert_eq!(bsc["returned"], 1);
    assert_eq!(bsc["count"], 1);
    assert_eq!(bsc["has_more"], true);
    assert!(bsc["next_cursor"].is_string());
    assert_eq!(bsc["incomplete_blocks"], json!([]));

    // W6: MCP append_blocks defaults to compact {ok, appended, block_ids}; compact:false returns blocks
    let append_compact = mcp::handle_request_value(
        &cfg,
        &auth,
        &client,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": { "name": "append_blocks", "arguments": { "page": "page-roadmap", "markdown": "- Item A\n- Item B" } }
        }),
    )
    .await
    .unwrap();
    assert_eq!(append_compact["result"]["isError"], false, "{append_compact}");
    let ac = &append_compact["result"]["structuredContent"];
    assert_eq!(ac["ok"], true);
    assert_eq!(ac["appended"], 2);
    assert!(ac.get("blocks").is_none());
    assert_eq!(ac["block_ids"].as_array().unwrap().len(), 2);

    let append_full = mcp::handle_request_value(
        &cfg,
        &auth,
        &client,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": { "name": "append_blocks", "arguments": { "page": "page-roadmap", "markdown": "- Item C", "compact": false } }
        }),
    )
    .await
    .unwrap();
    assert_eq!(append_full["result"]["isError"], false, "{append_full}");
    let af = &append_full["result"]["structuredContent"];
    assert_eq!(af["ok"], true);
    assert_eq!(af["appended"], 1);
    assert_eq!(af["blocks"].as_array().unwrap().len(), 1);

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn title_writes_need_one_exact_active_match() {
    let ws1 = "22222222-2222-2222-2222-222222222221";
    let ws2 = "22222222-2222-2222-2222-222222222222";
    let launch_id = "11111111-1111-1111-1111-111111111111";
    let sprint_retro_id = "44444444-4444-4444-4444-444444444441";
    let sprint_exact_id = "44444444-4444-4444-4444-444444444442";
    let row_id = "55555555-5555-5555-5555-555555555551";
    let db_id = "dddddddd-dddd-dddd-dddd-ddddddddddd1";

    let mock = ReadContractMock::default();
    mock.filter_search.store(true, Ordering::SeqCst);
    {
        let p_ws1 = page_obj(ws1, "Weekly Sync", "2026-11-22T12:00:00.000Z");
        let p_ws2 = page_obj(ws2, "Weekly Sync", "2026-11-22T12:05:00.000Z");
        let p_launch = page_obj(launch_id, "Q4 Launch Plan", "2026-11-20T00:00:00.000Z");
        let p_sprint_retro =
            page_obj(sprint_retro_id, "Sprint Retrospective", "2026-11-20T00:00:00.000Z");
        let p_row = db_row_obj(row_id, db_id, "Ship CLI Audit", "Todo", "2026-11-20T00:00:00.000Z");
        let mut pages = mock.pages.lock().unwrap();
        for p in [&p_ws1, &p_ws2, &p_launch, &p_sprint_retro, &p_row] {
            pages.insert(p["id"].as_str().unwrap().into(), p.clone());
        }
        let mut items = mock.search_items.lock().unwrap();
        items.push(p_ws2);
        items.push(p_ws1);
        items.push(p_launch);
        items.push(p_sprint_retro);
        items.push(p_row);
    }
    {
        let mut blocks = mock.blocks.lock().unwrap();
        for id in [ws1, ws2, launch_id, sprint_retro_id, sprint_exact_id] {
            blocks.insert(id.into(), vec![paragraph_block(&format!("blk-{id}"), "Body")]);
        }
    }

    let (base_url, server) = start_mock_server(mock.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");

    // 1. Duplicate exact titles (same search page) -> ambiguous_page, 2 candidates, 0 PATCHes
    let dup = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "append", "Weekly Sync", "x", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(dup.status.code(), Some(1));
    let dup_v: Value = serde_json::from_slice(&dup.stdout).unwrap();
    assert_eq!(dup_v["ok"], false);
    assert_eq!(dup_v["error"]["code"], "ambiguous_page");
    assert_eq!(
        dup_v["error"]["details"]["candidates"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(mock.block_patch_reqs.lock().unwrap().is_empty());

    // 2. 2-page duplicate control: page 1 has 1 exact match, page 2 has 1 exact match
    mock.search_page_limit.store(1, Ordering::SeqCst);
    mock.search_reqs.store(0, Ordering::SeqCst);
    let dup_2p = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "append", "Weekly Sync", "x", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(dup_2p.status.code(), Some(1));
    let dup_2p_v: Value = serde_json::from_slice(&dup_2p.stdout).unwrap();
    assert_eq!(dup_2p_v["error"]["code"], "ambiguous_page");
    assert_eq!(mock.search_reqs.load(Ordering::SeqCst), 2);
    assert!(mock.block_patch_reqs.lock().unwrap().is_empty());

    // Same 2-page mock, rename page 2 so only page 1 matches -> exhausts search & patches page 1
    {
        let mut items = mock.search_items.lock().unwrap();
        for item in items.iter_mut() {
            if item["id"] == ws1 {
                item["properties"]["title"]["title"][0]["plain_text"] =
                    json!("Weekly Sync Archive");
            }
        }
        mock.pages.lock().unwrap().get_mut(ws1).unwrap()["properties"]["title"]["title"][0]
            ["plain_text"] = json!("Weekly Sync Archive");
    }
    mock.search_reqs.store(0, Ordering::SeqCst);
    let unique_2p = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "append", "Weekly Sync", "unique-2p", "--json"])
        .output()
        .await
        .unwrap();
    assert!(
        unique_2p.status.success(),
        "{}",
        String::from_utf8_lossy(&unique_2p.stderr)
    );
    assert_eq!(mock.search_reqs.load(Ordering::SeqCst), 2);
    let patches = std::mem::take(&mut *mock.block_patch_reqs.lock().unwrap());
    assert_eq!(patches.len(), 1);
    assert_eq!(patches[0].0, ws2);

    // Restore ws1 title to "Weekly Sync" and remove page size cap for next checks
    mock.search_page_limit.store(0, Ordering::SeqCst);
    {
        let mut items = mock.search_items.lock().unwrap();
        for item in items.iter_mut() {
            if item["id"] == ws1 {
                item["properties"]["title"]["title"][0]["plain_text"] = json!("Weekly Sync");
            }
        }
        mock.pages.lock().unwrap().get_mut(ws1).unwrap()["properties"]["title"]["title"][0]
            ["plain_text"] = json!("Weekly Sync");
    }

    // 3. Partial cache: fresh data dir, read only ws1 via UUID, then write by title -> still ambiguous_page
    let tmp_partial = tempfile::tempdir().unwrap();
    let p_cfg = tmp_partial.path().join("cfg");
    let p_data = tmp_partial.path().join("data");
    let get1 = notion_cli_cmd(&base_url, &p_cfg, &p_data)
        .args(["pages", "get", ws1, "--json"])
        .output()
        .await
        .unwrap();
    assert!(get1.status.success());
    let dup_partial = notion_cli_cmd(&base_url, &p_cfg, &p_data)
        .args(["pages", "append", "Weekly Sync", "x", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(dup_partial.status.code(), Some(1));
    let dup_pv: Value = serde_json::from_slice(&dup_partial.stdout).unwrap();
    assert_eq!(dup_pv["error"]["code"], "ambiguous_page");
    assert!(mock.block_patch_reqs.lock().unwrap().is_empty());

    // 4. Stale cache row: ws1 is cached as "Weekly Sync", then renamed remotely to "Old Notes"
    // so search doesn't return ws1 -> GET /v1/pages/{ws1} verifies & drops it, then patches ws2
    {
        let mut items = mock.search_items.lock().unwrap();
        for item in items.iter_mut() {
            if item["id"] == ws1 {
                item["properties"]["title"]["title"][0]["plain_text"] = json!("Old Notes");
            }
        }
        mock.pages.lock().unwrap().get_mut(ws1).unwrap()["properties"]["title"]["title"][0]
            ["plain_text"] = json!("Old Notes");
    }
    mock.page_get_reqs.store(0, Ordering::SeqCst);
    let stale_ok = notion_cli_cmd(&base_url, &p_cfg, &p_data)
        .args(["pages", "append", "Weekly Sync", "after-rename", "--json"])
        .output()
        .await
        .unwrap();
    assert!(
        stale_ok.status.success(),
        "{}",
        String::from_utf8_lossy(&stale_ok.stderr)
    );
    assert_eq!(mock.page_get_reqs.load(Ordering::SeqCst), 1);
    let patches = std::mem::take(&mut *mock.block_patch_reqs.lock().unwrap());
    assert_eq!(patches.len(), 1);
    assert_eq!(patches[0].0, ws2);

    // 5. Archived + active with same title -> write hits active; --unarchive restores cached archived
    {
        let mut items = mock.search_items.lock().unwrap();
        for item in items.iter_mut() {
            if item["id"] == ws1 {
                item["properties"]["title"]["title"][0]["plain_text"] = json!("Weekly Sync");
                item["archived"] = json!(false);
            }
            if item["id"] == ws2 {
                item["archived"] = json!(true);
            }
        }
        let mut pages = mock.pages.lock().unwrap();
        pages.get_mut(ws1).unwrap()["properties"]["title"]["title"][0]["plain_text"] =
            json!("Weekly Sync");
        pages.get_mut(ws1).unwrap()["archived"] = json!(false);
        pages.get_mut(ws2).unwrap()["archived"] = json!(true);
    }
    let active_write = notion_cli_cmd(&base_url, &p_cfg, &p_data)
        .args(["pages", "append", "Weekly Sync", "active-only", "--json"])
        .output()
        .await
        .unwrap();
    assert!(
        active_write.status.success(),
        "{}",
        String::from_utf8_lossy(&active_write.stderr)
    );
    let patches = std::mem::take(&mut *mock.block_patch_reqs.lock().unwrap());
    assert_eq!(patches.len(), 1);
    assert_eq!(patches[0].0, ws1);

    // Remove ws2 from search results (simulating Notion Search omitting archived pages)
    // and ensure ws2 is cached as archived -> --unarchive verifies via GET and restores ws2
    {
        let mut items = mock.search_items.lock().unwrap();
        items.retain(|i| i["id"] != ws2);
    }
    let _ = notion_cli_cmd(&base_url, &p_cfg, &p_data)
        .args(["pages", "get", ws2, "--json"])
        .output()
        .await
        .unwrap();
    mock.page_get_reqs.store(0, Ordering::SeqCst);
    let unarch = notion_cli_cmd(&base_url, &p_cfg, &p_data)
        .args(["pages", "archive", "Weekly Sync", "--unarchive", "--json"])
        .output()
        .await
        .unwrap();
    assert!(
        unarch.status.success(),
        "{}",
        String::from_utf8_lossy(&unarch.stderr)
    );
    assert_eq!(mock.page_get_reqs.load(Ordering::SeqCst), 1);
    let page_patches = std::mem::take(&mut *mock.page_patch_reqs.lock().unwrap());
    assert_eq!(page_patches.len(), 1);
    assert_eq!(page_patches[0].0, ws2);
    assert_eq!(page_patches[0].1["archived"], false);

    // --unarchive when NOT in local cache -> page_not_found with ID/URL guidance
    let tmp_empty = tempfile::tempdir().unwrap();
    let unarch_miss = notion_cli_cmd(
        &base_url,
        &tmp_empty.path().join("cfg"),
        &tmp_empty.path().join("data"),
    )
    .args(["pages", "archive", "Weekly Sync", "--unarchive", "--json"])
    .output()
    .await
    .unwrap();
    assert_eq!(unarch_miss.status.code(), Some(1));
    let unarch_v: Value = serde_json::from_slice(&unarch_miss.stdout).unwrap();
    assert_eq!(unarch_v["error"]["code"], "page_not_found");
    assert!(unarch_v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("pass the page ID or URL"));

    // 6. Controls: UUID input -> 0 searches, 0 extra page GETs; unique title -> 1 search, 0 extra GETs
    mock.search_reqs.store(0, Ordering::SeqCst);
    mock.page_get_reqs.store(0, Ordering::SeqCst);
    let uuid_write = notion_cli_cmd(
        &base_url,
        &tmp_empty.path().join("cfg"),
        &tmp_empty.path().join("data"),
    )
    .args(["pages", "append", launch_id, "direct-uuid", "--json"])
    .output()
    .await
    .unwrap();
    assert!(uuid_write.status.success());
    assert_eq!(mock.search_reqs.load(Ordering::SeqCst), 0);
    assert_eq!(mock.page_get_reqs.load(Ordering::SeqCst), 0);
    let _ = std::mem::take(&mut *mock.block_patch_reqs.lock().unwrap());

    let unique_write = notion_cli_cmd(
        &base_url,
        &tmp_empty.path().join("cfg"),
        &tmp_empty.path().join("data"),
    )
    .args(["pages", "append", "Q4 Launch Plan", "unique-title", "--json"])
    .output()
    .await
    .unwrap();
    assert!(unique_write.status.success());
    assert_eq!(mock.search_reqs.load(Ordering::SeqCst), 1);
    assert_eq!(mock.page_get_reqs.load(Ordering::SeqCst), 0);
    let _ = std::mem::take(&mut *mock.block_patch_reqs.lock().unwrap());

    // 7. Substring write rejected with page_not_found + candidates, while substring read succeeds
    let sub_write = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "append", "Launch", "x", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(sub_write.status.code(), Some(1));
    let sub_v: Value = serde_json::from_slice(&sub_write.stdout).unwrap();
    assert_eq!(sub_v["error"]["code"], "page_not_found");
    assert_eq!(
        sub_v["error"]["details"]["candidates"][0]["title"],
        "Q4 Launch Plan"
    );
    assert!(mock.block_patch_reqs.lock().unwrap().is_empty());

    let sub_read = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "Launch", "--json"])
        .output()
        .await
        .unwrap();
    assert!(sub_read.status.success());
    let sr_v: Value = serde_json::from_slice(&sub_read.stdout).unwrap();
    assert_eq!(sr_v["id"], launch_id);

    // Online missing title read fails locally with page_not_found and 0 GET /v1/pages/* calls
    mock.page_get_reqs.store(0, Ordering::SeqCst);
    let missing_read = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "No Such Page Title", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(missing_read.status.code(), Some(1));
    let mr_v: Value = serde_json::from_slice(&missing_read.stdout).unwrap();
    assert_eq!(mr_v["error"]["code"], "page_not_found");
    assert_eq!(mock.page_get_reqs.load(Ordering::SeqCst), 0);

    // F4b resolver read: cached "Sprint Retrospective" must not shadow remote exact "Sprint"
    let _ = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", sprint_retro_id, "--json"])
        .output()
        .await
        .unwrap();
    {
        let p_sprint = page_obj(sprint_exact_id, "Sprint", "2026-11-22T10:00:00.000Z");
        mock.pages
            .lock()
            .unwrap()
            .insert(sprint_exact_id.into(), p_sprint.clone());
        mock.search_items.lock().unwrap().push(p_sprint);
    }
    let exact_read = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "Sprint", "--json"])
        .output()
        .await
        .unwrap();
    assert!(exact_read.status.success());
    let er_v: Value = serde_json::from_slice(&exact_read.stdout).unwrap();
    assert_eq!(er_v["id"], sprint_exact_id);

    // F5c: db update-item accepts Notion URL
    let url_update = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "db",
            "update-item",
            "https://www.notion.so/Ship-CLI-55555555555555555555555555555551",
            "--status",
            "Done",
            "--json",
        ])
        .output()
        .await
        .unwrap();
    assert!(
        url_update.status.success(),
        "{}",
        String::from_utf8_lossy(&url_update.stderr)
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partial_cache_does_not_answer_lists() {
    let mock = standard_mock();
    let expected_pages = mock.pages.lock().unwrap().len();
    let expected_dbs = mock.databases.lock().unwrap().len();
    let (base_url, server) = start_mock_server(mock.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");

    // 1. Single page get creates a partial cache (no workspace-complete marker)
    let get_one = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-roadmap", "--json"])
        .output()
        .await
        .unwrap();
    assert!(get_one.status.success());

    // 2. pages list must sync full workspace rather than returning count: 1
    let before = mock.search_reqs.load(Ordering::SeqCst);
    let list1 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "list", "--json"])
        .output()
        .await
        .unwrap();
    assert!(list1.status.success());
    let v1: Value = serde_json::from_slice(&list1.stdout).unwrap();
    assert_eq!(v1["count"], expected_pages);
    assert!(mock.search_reqs.load(Ordering::SeqCst) > before);

    // 3. Second pages list hits complete cache with 0 additional search requests (control)
    let after_first = mock.search_reqs.load(Ordering::SeqCst);
    let list2 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "list", "--json"])
        .output()
        .await
        .unwrap();
    assert!(list2.status.success());
    let v2: Value = serde_json::from_slice(&list2.stdout).unwrap();
    assert_eq!(v2["count"], expected_pages);
    assert_eq!(mock.search_reqs.load(Ordering::SeqCst), after_first);

    // 4. Separate fresh dir: single db get must not cause db list to return count: 1
    let tmp2 = tempfile::tempdir().unwrap();
    let db_get = notion_cli_cmd(
        &base_url,
        &tmp2.path().join("cfg"),
        &tmp2.path().join("data"),
    )
    .args(["db", "get", "db-tasks", "--json"])
    .output()
    .await
    .unwrap();
    assert!(db_get.status.success());
    let db_list = notion_cli_cmd(
        &base_url,
        &tmp2.path().join("cfg"),
        &tmp2.path().join("data"),
    )
    .args(["db", "list", "--json"])
    .output()
    .await
    .unwrap();
    assert!(db_list.status.success());
    let db_v: Value = serde_json::from_slice(&db_list.stdout).unwrap();
    assert_eq!(db_v["count"], expected_dbs);

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_append_and_export_compact_defaults_and_full_flag() {
    let mock = standard_mock();
    let (base_url, server) = start_mock_server(mock).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");

    // pages append --json defaults to compact {ok, appended, block_ids}
    let app_compact = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "append", "page-roadmap", "- One\n- Two", "--json"])
        .output()
        .await
        .unwrap();
    assert!(
        app_compact.status.success(),
        "{}",
        String::from_utf8_lossy(&app_compact.stderr)
    );
    let ac: Value = serde_json::from_slice(&app_compact.stdout).unwrap();
    assert_eq!(ac["ok"], true);
    assert_eq!(ac["appended"], 2);
    assert!(ac.get("blocks").is_none());
    assert_eq!(ac["block_ids"].as_array().unwrap().len(), 2);

    // pages append --json --full returns blocks
    let app_full = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "append", "page-roadmap", "- Three", "--json", "--full"])
        .output()
        .await
        .unwrap();
    assert!(app_full.status.success());
    let af: Value = serde_json::from_slice(&app_full.stdout).unwrap();
    assert_eq!(af["ok"], true);
    assert_eq!(af["appended"], 1);
    assert_eq!(af["blocks"].as_array().unwrap().len(), 1);

    // blocks append --json defaults to compact; --full restores blocks
    let blk_compact = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["blocks", "append", "page-roadmap", "- Four", "--json"])
        .output()
        .await
        .unwrap();
    assert!(blk_compact.status.success());
    let bc: Value = serde_json::from_slice(&blk_compact.stdout).unwrap();
    assert!(bc.get("blocks").is_none());
    assert_eq!(bc["block_ids"].as_array().unwrap().len(), 1);

    // pages export --out --json omits markdown and includes bytes
    let out_file = tmp.path().join("exported.md");
    let exp_out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "pages",
            "export",
            "page-roadmap",
            "--out",
            out_file.to_str().unwrap(),
            "--json",
        ])
        .output()
        .await
        .unwrap();
    assert!(exp_out.status.success());
    let eo: Value = serde_json::from_slice(&exp_out.stdout).unwrap();
    assert_eq!(eo["ok"], true);
    assert!(eo.get("markdown").is_none());
    assert!(eo["bytes"].as_u64().unwrap() > 0);
    assert!(out_file.exists());

    // pages export --json (without --out) keeps markdown
    let exp_stdout = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "export", "page-roadmap", "--json"])
        .output()
        .await
        .unwrap();
    assert!(exp_stdout.status.success());
    let es: Value = serde_json::from_slice(&exp_stdout.stdout).unwrap();
    assert!(es["markdown"].as_str().unwrap().contains("Roadmap"));

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_and_mcp_list_users_return_workspace_users() {
    let mock = standard_mock();
    let (base_url, server) = start_mock_server(mock).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");

    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["users", "list", "--json"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let val: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(val["ok"], true);
    assert_eq!(val["count"], 1);
    assert_eq!(val["users"][0]["id"], "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaa1");
    assert_eq!(val["users"][0]["name"], "Alice Kim");

    let text_out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["users", "list"])
        .output()
        .await
        .unwrap();
    assert!(text_out.status.success());
    let stdout = String::from_utf8_lossy(&text_out.stdout);
    assert!(stdout.contains("Alice Kim <alice@example.com> (aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaa1)"));

    let (cfg, auth, client, storage) = mcp_runtime(&base_url);
    let mcp_users = mcp::handle_request_value(
        &cfg,
        &auth,
        &client,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": { "name": "list_users", "arguments": {} }
        }),
    )
    .await
    .unwrap();
    assert_eq!(mcp_users["result"]["isError"], false, "{mcp_users}");
    assert_eq!(
        mcp_users["result"]["structuredContent"]["users"][0]["id"],
        "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaa1"
    );

    server.abort();
}
