//! Standalone `@bitefoundry/notion` (`notion`) CLI parser, command dispatcher,
//! and shared domain operations used across CLI, HTTP API (`serve`), and MCP (`mcp`).

mod args;
mod blocks;
mod databases;
mod format;
mod pages;
mod workspace;

#[cfg(test)]
mod tests;

use std::io::{self, IsTerminal, Write};

use anyhow::{bail, Result};
use clap::{CommandFactory, FromArgMatches};
use serde_json::json;

use crate::{
    auth::{AuthManager, OAuthLoginOptions},
    config::{self, AppConfig},
    notion::{media, NotionClient},
    safe_text::sanitize_terminal_inline,
    serve,
    storage::Storage,
    sync, tui, updater,
};

pub use self::{
    args::{
        parse_cli_from, try_fast_help_or_version, BlocksCommand, Cli, Command, CommentsCommand,
        DaemonCommand, DbCommand, PagesCommand, UsersCommand, WorkspacesCommand,
    },
    blocks::{
        op_append_blocks, op_delete_block, op_diff_sync_page_markdown, op_list_blocks,
        op_replace_page_markdown, op_toggle_todo, op_update_block_text,
    },
    databases::{
        op_create_database_item, op_get_database, op_list_databases, op_query_database,
        op_update_database_item,
    },
    format::{list_envelope, ListPage},
    pages::{
        op_archive_page, op_create_page, op_get_page, op_get_page_compact, op_get_page_fields,
        op_get_page_for_output, op_get_page_with_options, op_list_pages, op_patch_page, PagePatch,
    },
    workspace::{
        op_add_comment, op_list_comments, op_list_users, op_list_workspaces, op_search, op_whoami,
    },
};
pub(crate) use self::pages::op_update_page_metadata;

use self::{
    args::{error_envelope, leaf_json_flag, normalize_cli_args},
    blocks::handle_blocks_cmd,
    databases::handle_db_cmd,
    format::print_json,
    pages::handle_pages_cmd,
    workspace::{
        handle_comments_cmd, handle_daemon_cmd, handle_doctor_cmd, handle_users_cmd,
        handle_workspaces_cmd,
    },
};

#[cfg(test)]
use self::format::offset_page;

pub async fn run_from_env() -> Result<()> {
    let args = normalize_cli_args(std::env::args_os().collect());
    let matches = Cli::command()
        .try_get_matches_from(args)
        .unwrap_or_else(|e| e.exit());
    let wants_json = leaf_json_flag(&matches);
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    let res = run_cli(cli).await;
    if wants_json {
        if let Err(err) = &res {
            print_json(&error_envelope(err))?;
        }
    }
    res
}

pub async fn run_cli(cli: Cli) -> Result<()> {
    let mut config = AppConfig::load();
    if let Some(ref ws) = cli.workspace {
        config.override_workspace(ws.clone());
    }
    let workspace_override = cli.workspace.clone();

    let Some(command) = cli.command else {
        return launch_tui(config, workspace_override).await;
    };

    match command {
        Command::Tui => launch_tui(config, workspace_override).await,
        Command::Login {
            token,
            oauth,
            client_id,
            client_secret,
            port,
            redirect_uri,
            no_browser,
            no_qr,
            json: as_json,
        } => {
            let auth = AuthManager::new(config.clone(), workspace_override.clone())?;
            if oauth {
                let cid = client_id
                    .or_else(|| std::env::var("NOTION_OAUTH_CLIENT_ID").ok())
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| config.oauth_client_id.clone());
                let csec = client_secret
                    .or_else(|| std::env::var("NOTION_OAUTH_CLIENT_SECRET").ok())
                    .unwrap_or_default();
                let cred = auth
                    .login_with_oauth(OAuthLoginOptions {
                        client_id: cid,
                        client_secret: csec,
                        workspace_alias: workspace_override,
                        port,
                        redirect_uri,
                        no_browser,
                        no_qr,
                    })
                    .await?;
                if as_json {
                    print_json(&json!({
                        "ok": true,
                        "auth_mode": cred.auth_mode,
                        "workspace_profile": cred.workspace_profile,
                        "workspace_id": cred.workspace_id,
                        "workspace_name": cred.workspace_name,
                        "workspace_icon": cred.workspace_icon,
                        "bot_id": cred.bot_id,
                    }))?;
                } else {
                    println!(
                        "Connected via OAuth to workspace '{}' (profile: {})",
                        sanitize_terminal_inline(&cred.workspace_name),
                        sanitize_terminal_inline(&cred.workspace_profile)
                    );
                }
            } else {
                let raw_tok = if let Some(t) = token {
                    t
                } else if let Ok(env_t) = std::env::var("NOTION_TOKEN") {
                    if !env_t.trim().is_empty() {
                        env_t
                    } else {
                        prompt_token_interactive(no_browser, no_qr)?
                    }
                } else {
                    prompt_token_interactive(no_browser, no_qr)?
                };
                let (cred, me) = auth
                    .login_with_token(&raw_tok, workspace_override.as_deref())
                    .await?;
                if as_json {
                    print_json(&json!({
                        "ok": true,
                        "auth_mode": cred.auth_mode,
                        "workspace_profile": cred.workspace_profile,
                        "workspace_id": cred.workspace_id,
                        "workspace_name": cred.workspace_name,
                        "bot": me,
                    }))?;
                } else {
                    println!(
                        "Authenticated as '{}' in workspace '{}' (profile: {})",
                        sanitize_terminal_inline(&me.name),
                        sanitize_terminal_inline(&cred.workspace_name),
                        sanitize_terminal_inline(&cred.workspace_profile)
                    );
                }
            }
            Ok(())
        }
        Command::Logout { json: as_json } => {
            let auth = AuthManager::new(config, workspace_override.clone())?;
            let removed = auth.logout(workspace_override.as_deref()).await?;
            if as_json {
                print_json(&json!({ "ok": true, "removed_workspace": removed }))?;
            } else {
                println!("Logged out workspace profile '{removed}'.");
            }
            Ok(())
        }
        Command::Whoami { json: as_json } => {
            let (auth, client, _storage) = open_runtime(&config, workspace_override).await?;
            let val = op_whoami(&config, &auth, &client).await?;
            if as_json {
                print_json(&val)?;
            } else {
                println!(
                    "Workspace: {} ({})\nUser/Bot:  {} ({})\nAuth Mode: {}",
                    sanitize_terminal_inline(val["workspace_name"].as_str().unwrap_or("default")),
                    sanitize_terminal_inline(val["workspace_profile"].as_str().unwrap_or("default")),
                    sanitize_terminal_inline(val["user"]["name"].as_str().unwrap_or("Unknown")),
                    sanitize_terminal_inline(val["user"]["user_type"].as_str().unwrap_or("bot")),
                    sanitize_terminal_inline(val["auth_mode"].as_str().unwrap_or("unknown")),
                );
            }
            Ok(())
        }
        Command::Workspaces { action } => handle_workspaces_cmd(config, action).await,
        Command::Search {
            query,
            remote,
            offline,
            limit,
            json: as_json,
        } => {
            let (_auth, client, storage) = open_runtime(&config, workspace_override).await?;
            let hits = op_search(&client, &storage, &query, remote, offline, limit).await?;
            if as_json {
                print_json(&json!({ "ok": true, "count": hits.len(), "results": hits }))?;
            } else if hits.is_empty() {
                println!(
                    "No matching Notion pages or databases for '{}'.",
                    sanitize_terminal_inline(&query)
                );
            } else {
                for h in hits {
                    let icon = h.icon.as_deref().unwrap_or(if h.item_type == "database" {
                        "🗂"
                    } else {
                        "📄"
                    });
                    println!(
                        "{} [{}] {} ({})",
                        sanitize_terminal_inline(icon),
                        sanitize_terminal_inline(&h.item_type),
                        sanitize_terminal_inline(&h.title),
                        sanitize_terminal_inline(&h.id)
                    );
                    if !h.snippet.trim().is_empty() {
                        println!("    {}", sanitize_terminal_inline(h.snippet.trim()));
                    }
                }
            }
            Ok(())
        }
        Command::Pages { action } => {
            let (_auth, client, storage) = open_runtime(&config, workspace_override).await?;
            handle_pages_cmd(&client, &storage, action).await
        }
        Command::Blocks { action } => {
            let (_auth, client, storage) = open_runtime(&config, workspace_override).await?;
            handle_blocks_cmd(&client, &storage, action).await
        }
        Command::Db { action } => {
            let (_auth, client, storage) = open_runtime(&config, workspace_override).await?;
            handle_db_cmd(&client, &storage, action).await
        }
        Command::Comments { action } => {
            let (_auth, client, storage) = open_runtime(&config, workspace_override).await?;
            handle_comments_cmd(&client, &storage, action).await
        }
        Command::Users { action } => {
            let (_auth, client, _storage) = open_runtime(&config, workspace_override).await?;
            handle_users_cmd(&client, action).await
        }
        Command::Download {
            target,
            out_dir,
            json: as_json,
        } => {
            let (_auth, client, _storage) = open_runtime(&config, workspace_override).await?;
            let res =
                media::download_media_target(&config, &client, &target, out_dir.as_deref()).await?;
            if as_json {
                print_json(&res)?;
            } else {
                println!(
                    "Downloaded {} ({} bytes, {}) -> {}",
                    sanitize_terminal_inline(&res.file_name),
                    res.size_bytes,
                    sanitize_terminal_inline(&res.mime_type),
                    sanitize_terminal_inline(&res.path)
                );
            }
            Ok(())
        }
        Command::Sync { full, json: as_json } => {
            let (_auth, client, storage) = open_runtime(&config, workspace_override).await?;
            let show_progress = !as_json && io::stderr().is_terminal();
            let started = std::time::Instant::now();
            let (ws_id, pages, databases, outcome) =
                sync::sync_workspace_tree_with_options(&client, &storage, full, |batch, p, d| {
                    if show_progress {
                        eprint!("\rSyncing workspace... batch {batch} ({p} pages, {d} databases)");
                        let _ = io::stderr().flush();
                    }
                })
                .await?;
            if show_progress {
                eprintln!(
                    "\rSynced in {:.1}s                                          ",
                    started.elapsed().as_secs_f64()
                );
            }
            if as_json {
                print_json(&json!({
                    "ok": true,
                    "workspace_id": ws_id,
                    "pages_count": pages.len(),
                    "databases_count": databases.len(),
                    "incremental": !full,
                    "fetched_count": outcome.fetched,
                    "complete": true,
                }))?;
            } else {
                println!(
                    "Synced workspace '{ws_id}': {} pages, {} databases.",
                    pages.len(),
                    databases.len()
                );
            }
            Ok(())
        }
        Command::Daemon { action } => handle_daemon_cmd(config, action).await,
        Command::Serve {
            host,
            port,
            api_key,
            allow_keyless,
            allowed_host,
            cors_origin,
        } => {
            let (auth, client, storage) = open_runtime(&config, workspace_override).await?;
            let args = serve::ServeArgs {
                api_key,
                allow_keyless,
                allowed_hosts: allowed_host,
                cors_origins: cors_origin,
            };
            serve::run(config, auth, client, storage, &host, port, args).await
        }
        Command::Mcp => {
            let (auth, client, storage) = open_runtime(&config, workspace_override).await?;
            crate::mcp::run(config, auth, client, storage).await
        }
        Command::Doctor {
            online,
            json: as_json,
        } => handle_doctor_cmd(&config, workspace_override, online, as_json).await,
        Command::Update {
            check,
            force,
            json: as_json,
        } => {
            let res = updater::check_or_apply_update(&config.github_repo, check, force).await?;
            if as_json {
                print_json(&res)?;
            } else if res.updated {
                println!(
                    "Updated notion {} -> {} ({})",
                    res.current_version,
                    res.latest_version,
                    res.binary_path.display()
                );
            } else if res.update_available {
                println!(
                    "Update available: {} -> {} ({})",
                    res.current_version, res.latest_version, res.release_url
                );
            } else {
                println!("notion {} is up to date.", res.current_version);
            }
            Ok(())
        }
    }
}

async fn launch_tui(config: AppConfig, workspace_override: Option<String>) -> Result<()> {
    let (auth, client, storage) = open_runtime(&config, workspace_override).await?;
    tui::run(config, auth, client, storage).await
}

async fn open_runtime(
    config: &AppConfig,
    workspace_override: Option<String>,
) -> Result<(AuthManager, NotionClient, Storage)> {
    let auth = AuthManager::new(config.clone(), workspace_override)?;
    let ws_id = auth.active_workspace_id().await;
    let storage = Storage::open(&config::db_path(Some(&ws_id)))?;
    let client = NotionClient::new(config, auth.clone())?;
    Ok((auth, client, storage))
}

fn prompt_token_interactive(no_browser: bool, no_qr: bool) -> Result<String> {
    if !io::stdin().is_terminal() {
        bail!("No token provided. Pass `--token <ntn_...>` or set `NOTION_TOKEN`.");
    }

    let url = crate::auth::NOTION_INTEGRATIONS_URL;
    let env_no_browser = std::env::var("NOTION_NO_BROWSER")
        .ok()
        .is_some_and(|v| matches!(v.trim(), "1" | "true" | "yes"));
    let opened = if !no_browser && !env_no_browser {
        open::that(url).is_ok()
    } else {
        false
    };

    eprintln!("\nConnect Notion Workspace");
    eprintln!("────────────────────────────────────────────────────────────");
    if opened {
        eprintln!("  Opened Notion Integrations in your browser:");
    } else {
        eprintln!("  Open Notion Integrations in your browser:");
    }
    eprintln!("  {url}");
    if !no_qr {
        if let Some(qr) = crate::auth::render_terminal_qr(url) {
            eprintln!("\n  Or scan with your phone:\n{qr}");
        }
    }
    eprintln!("  1. Click \"New integration\" (Internal) and copy the secret (`ntn_...`).");
    eprintln!("  2. In Notion, share target pages/databases with your integration (`···` -> Connections).");
    eprintln!("  (Prefer OAuth 2.0? Run `notion login --oauth --client-id <ID> --client-secret <SECRET>`)");
    eprintln!("────────────────────────────────────────────────────────────");
    eprint!("Paste Notion Internal Integration Token (ntn_... or secret_...): ");
    io::stderr().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let trimmed = line.trim().to_string();
    if trimmed.is_empty() {
        bail!("Token cannot be empty");
    }
    Ok(trimmed)
}

