//! Workspace, comments, users, daemon, doctor CLI handlers and shared operations.

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::{
    auth::AuthManager,
    config::{self, AppConfig, WorkspaceProfile},
    daemon, img_preview,
    models::{coded, NotionComment, NotionSearchHit, NotionUser},
    notion::{NotionClient, SearchFilterObject},
    resolver,
    safe_text::{sanitize_terminal_inline, sanitize_terminal_text},
    storage::{notion::CachedComments, Storage},
    sync,
};

use super::{
    args::{CommentsCommand, DaemonCommand, UsersCommand, WorkspacesCommand},
    format::{
        list_envelope, offset_page, parse_offset_cursor, print_json, require_limit,
        search_hits_from_response, warn_more_results, ListPage,
    },
};

pub(super) async fn handle_workspaces_cmd(mut config: AppConfig, action: WorkspacesCommand) -> Result<()> {
    match action {
        WorkspacesCommand::List { json: as_json } => {
            let active = config.effective_workspace_name(None);
            if as_json {
                print_json(&json!({
                    "ok": true,
                    "active_workspace": active,
                    "workspaces": config.workspaces,
                }))?;
            } else if config.workspaces.is_empty() {
                println!("No workspaces configured yet. Run `notion login` to add one.");
            } else {
                for ws in &config.workspaces {
                    let mark = if ws.name.eq_ignore_ascii_case(&active) {
                        "*"
                    } else {
                        " "
                    };
                    let icon = ws.workspace_icon.as_deref().unwrap_or("🏢");
                    println!(
                        "{mark} {} {} — {} [{}] ({})",
                        sanitize_terminal_inline(icon),
                        sanitize_terminal_inline(&ws.name),
                        sanitize_terminal_inline(&ws.workspace_name),
                        sanitize_terminal_inline(&ws.auth_mode),
                        sanitize_terminal_inline(&ws.workspace_id)
                    );
                }
            }
            Ok(())
        }
        WorkspacesCommand::Use {
            name,
            json: as_json,
        } => {
            let Some(found) = config.find_workspace(&name).cloned() else {
                bail!("Workspace profile '{name}' not found. Run `notion workspaces list`.");
            };
            config.set_active_workspace(found.name.clone());
            config.save()?;
            if as_json {
                print_json(&json!({ "ok": true, "active_workspace": found.name, "profile": found }))?;
            } else {
                println!(
                    "Switched active workspace to '{}' ({}).",
                    sanitize_terminal_inline(&found.name),
                    sanitize_terminal_inline(&found.workspace_name)
                );
            }
            Ok(())
        }
        WorkspacesCommand::Add {
            name,
            token,
            json: as_json,
        } => {
            let auth = AuthManager::new(config, Some(name.clone()))?;
            let (cred, user) = auth.login_with_token(&token, Some(&name)).await?;
            if as_json {
                print_json(&json!({ "ok": true, "workspace": cred.workspace_profile, "bot": user }))?;
            } else {
                println!(
                    "Added workspace '{}' ({}) authenticated as '{}'.",
                    sanitize_terminal_inline(&cred.workspace_profile),
                    sanitize_terminal_inline(&cred.workspace_name),
                    sanitize_terminal_inline(&user.name)
                );
            }
            Ok(())
        }
        WorkspacesCommand::Remove {
            name,
            json: as_json,
        } => {
            let auth = AuthManager::new(config, Some(name.clone()))?;
            let removed = auth.logout(Some(&name)).await?;
            if as_json {
                print_json(&json!({ "ok": true, "removed": removed }))?;
            } else {
                println!("Removed workspace profile '{removed}'.");
            }
            Ok(())
        }
    }
}


pub(super) async fn handle_comments_cmd(
    client: &NotionClient,
    storage: &Storage,
    action: CommentsCommand,
) -> Result<()> {
    match action {
        CommentsCommand::List {
            page,
            offline,
            limit,
            cursor,
            json: as_json,
        } => {
            let effective_limit = limit.unwrap_or(if as_json { 50 } else { usize::MAX });
            let list = op_list_comments(
                client,
                storage,
                &page,
                offline,
                effective_limit,
                cursor.as_deref(),
            )
            .await?;
            if as_json {
                print_json(&list_envelope("comments", &list))?;
            } else if list.items.is_empty() {
                println!("No comments on page.");
                warn_more_results(&list);
            } else {
                for c in &list.items {
                    println!(
                        "[{}] {}: {}",
                        sanitize_terminal_inline(&c.created_time),
                        sanitize_terminal_inline(&c.author),
                        sanitize_terminal_text(&c.body)
                    );
                }
                warn_more_results(&list);
            }
            Ok(())
        }
        CommentsCommand::Add {
            page,
            text,
            discussion,
            json: as_json,
        } => {
            let comment =
                op_add_comment(client, storage, &page, discussion.as_deref(), &text).await?;
            if as_json {
                print_json(&comment)?;
            } else {
                println!(
                    "Added comment ({}) by {}.",
                    sanitize_terminal_inline(&comment.id),
                    sanitize_terminal_inline(&comment.author)
                );
            }
            Ok(())
        }
    }
}

pub(super) async fn handle_users_cmd(client: &NotionClient, action: UsersCommand) -> Result<()> {
    match action {
        UsersCommand::List {
            limit,
            cursor,
            json: as_json,
        } => {
            let effective_limit = limit.unwrap_or(if as_json { 50 } else { usize::MAX });
            let list = op_list_users(client, effective_limit, cursor.as_deref()).await?;
            if as_json {
                print_json(&list_envelope("users", &list))?;
            } else {
                for u in &list.items {
                    if let Some(email) = u.email.as_deref().filter(|s| !s.trim().is_empty()) {
                        println!(
                            "{} <{}> ({})",
                            sanitize_terminal_inline(&u.name),
                            sanitize_terminal_inline(email),
                            sanitize_terminal_inline(&u.id)
                        );
                    } else {
                        println!(
                            "{} ({})",
                            sanitize_terminal_inline(&u.name),
                            sanitize_terminal_inline(&u.id)
                        );
                    }
                }
                warn_more_results(&list);
            }
            Ok(())
        }
    }
}

pub(super) async fn handle_daemon_cmd(config: AppConfig, action: DaemonCommand) -> Result<()> {
    match action {
        DaemonCommand::Start { json: as_json } => {
            let status = daemon::ensure_daemon_started(&config).await?;
            if as_json {
                print_json(&status)?;
            } else {
                println!(
                    "Notion sync daemon running (pid: {:?}, transport: {:?})",
                    status.pid, status.transport
                );
            }
            Ok(())
        }
        DaemonCommand::Status { json: as_json } => {
            let status = daemon::query_daemon_status(&config).await?;
            if as_json {
                print_json(&status)?;
            } else if status.running {
                println!(
                    "Daemon running (pid: {:?}, clients: {}, summary: {})",
                    status.pid, status.attached_clients, status.sync_summary
                );
            } else {
                println!("Daemon is not running.");
            }
            Ok(())
        }
        DaemonCommand::Stop { json: as_json } => {
            let stopped = daemon::stop_daemon(&config).await?;
            if as_json {
                print_json(&json!({ "ok": true, "stopped": stopped }))?;
            } else if stopped {
                println!("Stopped Notion sync daemon.");
            } else {
                println!("Notion sync daemon was not running.");
            }
            Ok(())
        }
        DaemonCommand::Restart { json: as_json } => {
            let _ = daemon::stop_daemon(&config).await?;
            let status = daemon::ensure_daemon_started(&config).await?;
            if as_json {
                print_json(&status)?;
            } else {
                println!("Restarted Notion sync daemon (pid: {:?}).", status.pid);
            }
            Ok(())
        }
        DaemonCommand::Run => daemon::run_daemon_foreground(config).await,
    }
}

pub(super) async fn handle_doctor_cmd(
    config: &AppConfig,
    workspace_override: Option<String>,
    online: bool,
    as_json: bool,
) -> Result<()> {
    let auth = AuthManager::new(config.clone(), workspace_override)?;
    let ws_name = auth.active_workspace_name().await;
    let ws_id = auth.active_workspace_id().await;
    let cred = auth.current_credential().await;
    let db_file = config::db_path(Some(&ws_id));
    let storage_ok = Storage::open(&db_file).is_ok();
    let caps = img_preview::detect_graphics_caps(config.image_protocol);

    let online_probe = if online {
        let client = NotionClient::new(config, auth.clone())?;
        match client.get_me().await {
            Ok(me) => Some(json!({ "ok": true, "user": me })),
            Err(err) => Some(json!({ "ok": false, "error": err.to_string() })),
        }
    } else {
        None
    };
    // Extra root certificates from NOTION_EXTRA_CA_PEM, SSL_CERT_FILE and NODE_EXTRA_CA_CERTS.
    let extra_ca = &crate::http::extra_cas()?.reports;

    let report = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "config_file": config::config_file_path(),
        "data_dir": config::data_dir(),
        "db_path": db_file,
        "sqlite_ok": storage_ok,
        "api_base_url": config.effective_api_base_url(),
        "active_workspace": ws_name,
        "workspace_id": ws_id,
        "workspaces_count": config.workspaces.len(),
        "authenticated": cred.is_some(),
        "auth_mode": cred.as_ref().map(|c| c.auth_mode.clone()),
        "image_protocol": caps.protocol.as_str(),
        "cell_px": [caps.cell_px.0, caps.cell_px.1],
        "online_probe": online_probe,
        "extra_ca": extra_ca,
    });

    if as_json {
        print_json(&report)?;
    } else {
        println!("@bitefoundry/notion v{}", env!("CARGO_PKG_VERSION"));
        println!("  Config:         {}", config::config_file_path().display());
        println!("  SQLite DB:      {} (ok: {})", db_file.display(), storage_ok);
        println!(
            "  Workspace:      {} ({})",
            sanitize_terminal_inline(&ws_name),
            sanitize_terminal_inline(&ws_id)
        );
        println!("  Authenticated:  {}", cred.is_some());
        println!("  Image Protocol: {}", caps.protocol.label());
        for ca in extra_ca {
            match &ca.error {
                Some(error) => println!("  Extra CA:       {} ({}) skipped: {error}", ca.variable, ca.source),
                None => println!(
                    "  Extra CA:       {} ({}): {} certificate(s) trusted, {} skipped",
                    ca.variable, ca.source, ca.certificates, ca.skipped
                ),
            }
        }
        if let Some(probe) = &online_probe {
            println!("  Online Probe:   {probe}");
        }
    }
    Ok(())
}

pub async fn op_whoami(
    config: &AppConfig,
    auth: &AuthManager,
    client: &NotionClient,
) -> Result<Value> {
    let user = client.get_me().await?;
    let cred = auth.current_credential().await;
    let profile = auth.active_workspace_name().await;
    let ws_id = auth.active_workspace_id().await;
    Ok(json!({
        "ok": true,
        "workspace_profile": profile,
        "workspace_id": ws_id,
        "workspace_name": user
            .workspace_name
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
            .or_else(|| {
                cred.as_ref()
                    .map(|c| c.workspace_name.clone())
                    .filter(|s| !s.trim().is_empty())
            })
            .unwrap_or_else(|| profile.clone()),
        "auth_mode": cred.as_ref().map(|c| c.auth_mode.clone()).unwrap_or_else(|| "token".into()),
        "api_base_url": config.effective_api_base_url(),
        "user": user,
    }))
}

pub async fn op_list_users(
    client: &NotionClient,
    limit: usize,
    cursor: Option<&str>,
) -> Result<ListPage<NotionUser>> {
    require_limit(limit)?;
    let _ = parse_offset_cursor(cursor)?;
    let users = client.list_users().await?;
    offset_page(users, limit, cursor)
}

pub fn op_list_workspaces(config: &AppConfig) -> Value {
    let active = config.effective_workspace_name(None);
    let list: Vec<&WorkspaceProfile> = config.workspaces.iter().collect();
    json!({
        "ok": true,
        "active_workspace": active,
        "workspaces": list,
    })
}

pub async fn op_search(
    client: &NotionClient,
    storage: &Storage,
    query: &str,
    remote: bool,
    offline: bool,
    limit: usize,
) -> Result<Vec<NotionSearchHit>> {
    let ws_id = client.auth().active_workspace_id().await;
    if remote && !offline {
        let resp = client
            .search_all(Some(query), SearchFilterObject::All, limit)
            .await?;
        let _ = storage.upsert_pages(&resp.pages);
        let _ = storage.upsert_databases(&resp.databases);
        return Ok(search_hits_from_response(resp, limit));
    }

    let local = storage.search_local(&ws_id, query, limit)?;
    let complete = sync::workspace_cache_complete(storage, &ws_id);
    if offline || (!local.is_empty() && complete) {
        return Ok(local);
    }
    // Fallback to remote search when local cache is empty or incomplete
    let resp = client
        .search_all(Some(query), SearchFilterObject::All, limit)
        .await?;
    let _ = storage.upsert_pages(&resp.pages);
    let _ = storage.upsert_databases(&resp.databases);
    let refreshed = storage.search_local(&ws_id, query, limit)?;
    if !refreshed.is_empty() {
        return Ok(refreshed);
    }
    Ok(search_hits_from_response(resp, limit))
}


pub async fn op_list_comments(
    client: &NotionClient,
    storage: &Storage,
    page: &str,
    offline: bool,
    limit: usize,
    cursor: Option<&str>,
) -> Result<ListPage<NotionComment>> {
    require_limit(limit)?;
    let ws_id = client.auth().active_workspace_id().await;
    let id = resolver::resolve_page_id(storage, (!offline).then_some(client), &ws_id, page).await?;
    if offline {
        return match storage.load_cached_comments(&ws_id, &id)? {
            CachedComments::NotFetched => Err(coded(
                "not_cached",
                format!("Comments of '{page}' not found in offline cache (not fetched)"),
            )),
            CachedComments::Failed(err) => Err(coded(
                "not_cached",
                format!("Comments of '{page}' not found in offline cache (last fetch failed: {err})"),
            )),
            CachedComments::Complete(list) => offset_page(list, limit, cursor),
            CachedComments::Partial(list) => {
                let mut page_out = offset_page(list, limit, cursor)?;
                if !page_out.has_more {
                    page_out.has_more = true;
                    page_out.next_cursor = None;
                    page_out.hint = Some(format!(
                        "Only part of the comments is cached; read the rest online: notion comments list {id} --json --limit 100"
                    ));
                }
                Ok(page_out)
            }
        };
    }
    let (comments, has_more, next_cursor) = client.list_comments_page(&id, limit, cursor).await?;
    if cursor.is_none() && !has_more {
        let _ = storage.replace_page_comments(&ws_id, &id, &comments, true);
    }
    Ok(ListPage {
        items: comments,
        has_more,
        next_cursor,
        hint: None,
    })
}

pub async fn op_add_comment(
    client: &NotionClient,
    storage: &Storage,
    page: &str,
    discussion_id: Option<&str>,
    text: &str,
) -> Result<NotionComment> {
    let ws_id = client.auth().active_workspace_id().await;
    let id = resolver::resolve_page_id_for_write(storage, client, &ws_id, page, false).await?;
    let comment = client.add_comment(Some(&id), discussion_id, text).await?;
    let _ = storage.append_cached_comment(&ws_id, &id, &comment);
    Ok(comment)
}

