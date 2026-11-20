//! Standalone `@bitefoundry/notion` (`notion` / `nt`) CLI parser, command dispatcher,
//! and shared domain operations used across CLI, HTTP API (`serve`), and MCP (`mcp`).

use std::{
    fs,
    io::{self, IsTerminal, Write},
    path::PathBuf,
};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use futures::stream::{self, StreamExt};
use serde_json::{json, Value};

use crate::{
    auth::{AuthManager, OAuthLoginOptions},
    config::{self, AppConfig, WorkspaceProfile, DEFAULT_SERVE_PORT},
    daemon, img_preview,
    models::{
        normalize_notion_id, NotionBlock, NotionBlockKind, NotionComment, NotionDatabase,
        NotionPage, NotionPageDetail, NotionSearchHit,
    },
    notion::{media, NotionClient, SearchFilterObject},
    parser::{
        block_comparison_key, blocks_to_markdown, blocks_to_plain_text, collect_media_refs,
        is_preserved_anchor_block, markdown_to_notion_blocks, parse_inline_markdown_rich_text,
        parse_notion_block, parse_slash_command_blocks, partition_blocks_around_anchors,
    },
    resolver, serve,
    storage::Storage,
    sync, tui, updater,
};

#[derive(Debug, Parser)]
#[command(
    name = "notion",
    version,
    about = "Notion terminal client & AI agent toolkit (CLI · TUI · HTTP API · MCP)",
    long_about = "Notion-native terminal workspace: Pages, Interactive Blocks, Databases (Table/Board/List), Comments & Multi-Workspace.\nRun without arguments to open the interactive Notion TUI."
)]
pub struct Cli {
    /// Override active workspace profile name (or set NOTION_WORKSPACE)
    #[arg(long, short = 'w', global = true)]
    pub workspace: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum WorkspacesCommand {
    /// List configured Notion workspace profiles
    List {
        #[arg(long)]
        json: bool,
    },
    /// Switch the default active Notion workspace profile
    Use {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Add a named Notion workspace profile using an Internal Integration Token
    Add {
        name: String,
        #[arg(long)]
        token: String,
        #[arg(long)]
        json: bool,
    },
    /// Remove a Notion workspace profile and its stored credential
    Remove {
        name: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum PagesCommand {
    /// List synced/accessible Notion pages
    List {
        #[arg(long)]
        archived: bool,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Fetch and display a Notion page (metadata + rendered blocks)
    Get {
        page: String,
        #[arg(long, default_value = "md", value_parser = ["md", "plain", "blocks"])]
        format: String,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// Create a new Notion page under a parent page or database
    Create {
        #[arg(long)]
        parent: String,
        #[arg(long)]
        title: String,
        #[arg(long)]
        icon: Option<String>,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long)]
        database: bool,
        #[arg(long)]
        json: bool,
    },
    /// Append Markdown or slash-style blocks (`/h1`, `/todo`, `/callout`, ...) to a page
    Append {
        page: String,
        body: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Edit a Notion page title/icon or open its Markdown body in `$EDITOR`
    Edit {
        page: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        icon: Option<String>,
        #[arg(long)]
        editor: bool,
        #[arg(long)]
        json: bool,
    },
    /// Export a Notion page to clean Markdown (stdout or `--out <file>`)
    Export {
        page: String,
        #[arg(long, short = 'o')]
        out: Option<PathBuf>,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// Archive (or `--unarchive`) a Notion page
    Archive {
        page: String,
        #[arg(long)]
        unarchive: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum BlocksCommand {
    /// List child blocks of a page or block
    List {
        block_id: String,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// Append Markdown or slash-style blocks to a page or block
    Append {
        block_id: String,
        body: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Toggle a `to_do` block's checked state
    Toggle {
        block_id: String,
        /// Optional explicit checked state (`true` or `false`); flips current state if omitted
        #[arg(long)]
        checked: Option<bool>,
        #[arg(long)]
        json: bool,
    },
    /// Delete (archive) a Notion block
    Delete {
        block_id: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum DbCommand {
    /// List Notion databases
    List {
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// Inspect a Notion database schema & properties
    Get {
        database: String,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// Query rows of a Notion database with optional JSON filter & sorts
    Query {
        database: String,
        #[arg(long)]
        filter: Option<String>,
        #[arg(long)]
        sorts: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// Create a new row item inside a Notion database
    #[command(name = "create-item")]
    CreateItem {
        database: String,
        #[arg(long)]
        title: String,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        properties: Option<String>,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Update properties on an existing database row page
    #[command(name = "update-item")]
    UpdateItem {
        page_id: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        properties: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum CommentsCommand {
    /// List comments on a Notion page
    List {
        page: String,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// Add a comment to a Notion page or discussion thread
    Add {
        page: String,
        text: String,
        #[arg(long)]
        discussion: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    Start {
        #[arg(long)]
        json: bool,
    },
    Status {
        #[arg(long)]
        json: bool,
    },
    Stop {
        #[arg(long)]
        json: bool,
    },
    Restart {
        #[arg(long)]
        json: bool,
    },
    #[command(hide = true)]
    Run,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Sign in to Notion via Internal Integration Token (default) or Public OAuth 2.0 (`--oauth`)
    #[command(visible_alias = "in")]
    Login {
        /// Internal Integration Token (`ntn_...` or `secret_...`)
        #[arg(long)]
        token: Option<String>,
        /// Use Public OAuth 2.0 loopback flow instead of internal integration token
        #[arg(long)]
        oauth: bool,
        #[arg(long)]
        client_id: Option<String>,
        #[arg(long)]
        client_secret: Option<String>,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        redirect_uri: Option<String>,
        /// Do not auto-open the web browser during login
        #[arg(long)]
        no_browser: bool,
        /// Do not print a terminal QR code during login
        #[arg(long)]
        no_qr: bool,
        #[arg(long)]
        json: bool,
    },
    /// Remove stored credentials for the active (or `--workspace`) profile
    #[command(visible_alias = "out")]
    Logout {
        #[arg(long)]
        json: bool,
    },
    /// Show currently authenticated Notion bot/user and active workspace
    Whoami {
        #[arg(long)]
        json: bool,
    },
    /// Manage multi-workspace profiles (`list`, `use`, `add`, `remove`)
    #[command(visible_alias = "ws")]
    Workspaces {
        #[command(subcommand)]
        action: WorkspacesCommand,
    },
    /// Search Notion pages and databases (local SQLite FTS5 + Korean <3 char LIKE, or `--remote`)
    Search {
        query: String,
        #[arg(long)]
        remote: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long, default_value_t = 30)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Work with Notion pages (`list`, `get`, `create`, `append`, `edit`, `export`, `archive`)
    Pages {
        #[command(subcommand)]
        action: PagesCommand,
    },
    /// Work with Notion blocks (`list`, `append`, `toggle`, `delete`)
    Blocks {
        #[command(subcommand)]
        action: BlocksCommand,
    },
    /// Work with Notion databases (`list`, `get`, `query`, `create-item`, `update-item`)
    Db {
        #[command(subcommand)]
        action: DbCommand,
    },
    /// List or post Notion page comments
    Comments {
        #[command(subcommand)]
        action: CommentsCommand,
    },
    /// Download a Notion media URL or image/file/pdf block ID to local disk
    Download {
        target: String,
        #[arg(long)]
        out_dir: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Synchronize workspace pages & databases into local SQLite WAL + FTS5 cache
    Sync {
        /// Force a full rescan instead of incremental `last_edited_time` sync
        #[arg(long)]
        full: bool,
        #[arg(long)]
        json: bool,
    },
    /// Manage optional single-instance background sync daemon
    Daemon {
        #[command(subcommand)]
        action: DaemonCommand,
    },
    /// Start local HTTP JSON API server (default `127.0.0.1:8791`)
    #[command(visible_alias = "api")]
    Serve {
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        #[arg(long, default_value_t = DEFAULT_SERVE_PORT)]
        port: u16,
        #[arg(long)]
        api_key: Option<String>,
    },
    /// Run stdio JSON-RPC 2.0 MCP server (18 Notion tools)
    #[command(visible_alias = "mcp-server")]
    Mcp,
    /// Open the interactive Notion Terminal UI
    Tui,
    /// Diagnose configuration, SQLite WAL/FTS5, keyring, terminal image protocol, and optional online API check
    Doctor {
        #[arg(long, visible_alias = "probe")]
        online: bool,
        #[arg(long)]
        json: bool,
    },
    /// Check for or install the latest GitHub Release binary
    #[command(visible_alias = "self-update")]
    Update {
        #[arg(long)]
        check: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
}

pub fn try_fast_help_or_version() -> Option<Result<()>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 2 && matches!(args[1].as_str(), "-V" | "--version") {
        println!("notion {}", env!("CARGO_PKG_VERSION"));
        return Some(Ok(()));
    }
    None
}

pub async fn run_from_env() -> Result<()> {
    let cli = Cli::parse();
    run_cli(cli).await
}

pub async fn run_cli(cli: Cli) -> Result<()> {
    let mut config = AppConfig::load();
    if let Some(ref ws) = cli.workspace {
        config.active_workspace = ws.clone();
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
                        cred.workspace_name, cred.workspace_profile
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
                        me.name, cred.workspace_name, cred.workspace_profile
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
                    val["workspace_name"].as_str().unwrap_or("default"),
                    val["workspace_profile"].as_str().unwrap_or("default"),
                    val["user"]["name"].as_str().unwrap_or("Unknown"),
                    val["user"]["user_type"].as_str().unwrap_or("bot"),
                    val["auth_mode"].as_str().unwrap_or("unknown"),
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
                println!("No matching Notion pages or databases for '{query}'.");
            } else {
                for h in hits {
                    let icon = h.icon.as_deref().unwrap_or(if h.item_type == "database" {
                        "🗂"
                    } else {
                        "📄"
                    });
                    println!("{} [{}] {} ({})", icon, h.item_type, h.title, h.id);
                    if !h.snippet.trim().is_empty() {
                        println!("    {}", h.snippet.trim());
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
                    res.file_name, res.size_bytes, res.mime_type, res.path
                );
            }
            Ok(())
        }
        Command::Sync { full, json: as_json } => {
            let (_auth, client, storage) = open_runtime(&config, workspace_override).await?;
            let show_progress = !as_json && io::stderr().is_terminal();
            let started = std::time::Instant::now();
            let (ws_id, pages, databases) =
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
        } => {
            let (auth, client, storage) = open_runtime(&config, workspace_override).await?;
            serve::run(config, auth, client, storage, &host, port, api_key).await
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

async fn handle_workspaces_cmd(mut config: AppConfig, action: WorkspacesCommand) -> Result<()> {
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
                        "{mark} {icon} {} — {} [{}] ({})",
                        ws.name, ws.workspace_name, ws.auth_mode, ws.workspace_id
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
            config.active_workspace = found.name.clone();
            config.save()?;
            if as_json {
                print_json(&json!({ "ok": true, "active_workspace": found.name, "profile": found }))?;
            } else {
                println!("Switched active workspace to '{}' ({}).", found.name, found.workspace_name);
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
                    cred.workspace_profile, cred.workspace_name, user.name
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

async fn handle_pages_cmd(
    client: &NotionClient,
    storage: &Storage,
    action: PagesCommand,
) -> Result<()> {
    match action {
        PagesCommand::List {
            archived,
            refresh,
            offline,
            limit,
            json: as_json,
        } => {
            let mut pages = op_list_pages(client, storage, archived, refresh, offline).await?;
            pages.truncate(limit);
            if as_json {
                print_json(&json!({ "ok": true, "count": pages.len(), "pages": pages }))?;
            } else {
                for p in pages {
                    let fav = if p.is_favorite { "★ " } else { "" };
                    println!("{fav}{} {} ({})", p.display_icon(), p.display_title(), p.id);
                }
            }
            Ok(())
        }
        PagesCommand::Get {
            page,
            format,
            offline,
            json: as_json,
        } => {
            let detail = op_get_page(client, storage, &page, offline).await?;
            if as_json {
                print_json(&detail)?;
            } else {
                match format.as_str() {
                    "plain" => println!("{}", detail.page.plain_text),
                    "blocks" => print_json(&detail.blocks)?,
                    _ => {
                        println!("# {} {}\n", detail.page.display_icon(), detail.page.display_title());
                        println!("{}", detail.markdown);
                    }
                }
            }
            Ok(())
        }
        PagesCommand::Create {
            parent,
            title,
            icon,
            body,
            file,
            database,
            json: as_json,
        } => {
            let md = read_body_or_file(body, file)?;
            let created = op_create_page(
                client,
                storage,
                &parent,
                &title,
                icon.as_deref(),
                md.as_deref(),
                database,
                None,
            )
            .await?;
            if as_json {
                print_json(&created)?;
            } else {
                println!("Created page '{}' ({}) -> {}", created.title, created.id, created.url);
            }
            Ok(())
        }
        PagesCommand::Append {
            page,
            body,
            file,
            json: as_json,
        } => {
            let text = read_body_or_file(body, file)?
                .ok_or_else(|| anyhow::anyhow!("Provide content via positional body or --file"))?;
            let blocks = op_append_blocks(client, storage, &page, &text).await?;
            if as_json {
                print_json(&json!({ "ok": true, "appended": blocks.len(), "blocks": blocks }))?;
            } else {
                println!("Appended {} block(s).", blocks.len());
            }
            Ok(())
        }
        PagesCommand::Edit {
            page,
            title,
            icon,
            editor,
            json: as_json,
        } => {
            let ws_id = client.auth().active_workspace_id().await;
            let page_id = resolver::resolve_page_id(storage, Some(client), &ws_id, &page).await?;
            if editor {
                let detail = op_get_page(client, storage, &page_id, false).await?;
                let edited = crate::ui::composer::external_edit(&detail.markdown)
                    .context("Failed to edit page in external $EDITOR")?;
                if edited.trim() != detail.markdown.trim() {
                    let _ = op_replace_page_markdown(client, storage, &page_id, &edited).await?;
                }
            }
            let updated = if title.is_some() || icon.is_some() {
                op_update_page_meta(client, storage, &page_id, title.as_deref(), icon.as_deref())
                    .await?
            } else {
                let d = sync::sync_page_detail(client, storage, &page_id).await?;
                d.page
            };
            if as_json {
                print_json(&updated)?;
            } else {
                println!("Updated page '{}' ({}).", updated.title, updated.id);
            }
            Ok(())
        }
        PagesCommand::Export {
            page,
            out,
            offline,
            json: as_json,
        } => {
            let detail = op_get_page(client, storage, &page, offline).await?;
            let full_md = format!("# {}\n\n{}\n", detail.page.display_title(), detail.markdown);
            if let Some(ref path) = out {
                config::ensure_parent(path)?;
                fs::write(path, full_md.as_bytes())?;
            }
            if as_json {
                print_json(&json!({
                    "ok": true,
                    "page_id": detail.page.id,
                    "title": detail.page.title,
                    "path": out.map(|p| p.to_string_lossy().into_owned()),
                    "markdown": full_md,
                }))?;
            } else if let Some(path) = out {
                println!("Exported '{}' -> {}", detail.page.title, path.display());
            } else {
                print!("{full_md}");
            }
            Ok(())
        }
        PagesCommand::Archive {
            page,
            unarchive,
            json: as_json,
        } => {
            let updated = op_archive_page(client, storage, &page, !unarchive).await?;
            if as_json {
                print_json(&updated)?;
            } else {
                let verb = if unarchive { "Restored" } else { "Archived" };
                println!("{verb} page '{}' ({}).", updated.title, updated.id);
            }
            Ok(())
        }
    }
}

async fn handle_blocks_cmd(
    client: &NotionClient,
    storage: &Storage,
    action: BlocksCommand,
) -> Result<()> {
    match action {
        BlocksCommand::List {
            block_id,
            offline,
            json: as_json,
        } => {
            let blocks = op_list_blocks(client, storage, &block_id, offline).await?;
            if as_json {
                print_json(&json!({ "ok": true, "count": blocks.len(), "blocks": blocks }))?;
            } else {
                println!("{}", blocks_to_markdown(&blocks));
            }
            Ok(())
        }
        BlocksCommand::Append {
            block_id,
            body,
            file,
            json: as_json,
        } => {
            let text = read_body_or_file(body, file)?
                .ok_or_else(|| anyhow::anyhow!("Provide content via positional body or --file"))?;
            let blocks = op_append_blocks(client, storage, &block_id, &text).await?;
            if as_json {
                print_json(&json!({ "ok": true, "appended": blocks.len(), "blocks": blocks }))?;
            } else {
                println!("Appended {} block(s) to {block_id}.", blocks.len());
            }
            Ok(())
        }
        BlocksCommand::Toggle {
            block_id,
            checked,
            json: as_json,
        } => {
            let blk = op_toggle_todo(client, storage, &block_id, checked).await?;
            if as_json {
                print_json(&blk)?;
            } else {
                let state = match blk.kind {
                    NotionBlockKind::ToDo { checked, .. } => {
                        if checked {
                            "[x]"
                        } else {
                            "[ ]"
                        }
                    }
                    _ => "[?]",
                };
                println!("{state} {} ({})", blk.plain_text(), blk.id);
            }
            Ok(())
        }
        BlocksCommand::Delete {
            block_id,
            json: as_json,
        } => {
            let resp = client.delete_block(&block_id).await?;
            if as_json {
                print_json(&json!({ "ok": true, "deleted": block_id, "response": resp }))?;
            } else {
                println!("Deleted block {block_id}.");
            }
            Ok(())
        }
    }
}

async fn handle_db_cmd(
    client: &NotionClient,
    storage: &Storage,
    action: DbCommand,
) -> Result<()> {
    match action {
        DbCommand::List {
            refresh,
            offline,
            json: as_json,
        } => {
            let dbs = op_list_databases(client, storage, refresh, offline).await?;
            if as_json {
                print_json(&json!({ "ok": true, "count": dbs.len(), "databases": dbs }))?;
            } else {
                for db in dbs {
                    println!("{} {} ({})", db.display_icon(), db.display_title(), db.id);
                }
            }
            Ok(())
        }
        DbCommand::Get {
            database,
            offline,
            json: as_json,
        } => {
            let db = op_get_database(client, storage, &database, offline).await?;
            if as_json {
                print_json(&db)?;
            } else {
                println!("{} {} ({})", db.display_icon(), db.display_title(), db.id);
                for (name, prop) in &db.schema {
                    println!("  - {name}: {}", prop.prop_type);
                }
            }
            Ok(())
        }
        DbCommand::Query {
            database,
            filter,
            sorts,
            limit,
            offline,
            json: as_json,
        } => {
            let filter_val = filter
                .as_deref()
                .map(serde_json::from_str::<Value>)
                .transpose()
                .context("Invalid --filter JSON")?;
            let sorts_val = sorts
                .as_deref()
                .map(serde_json::from_str::<Value>)
                .transpose()
                .context("Invalid --sorts JSON")?;
            let rows = op_query_database(
                client,
                storage,
                &database,
                filter_val,
                sorts_val,
                limit,
                offline,
            )
            .await?;
            if as_json {
                print_json(&json!({ "ok": true, "count": rows.len(), "rows": rows }))?;
            } else {
                for row in rows {
                    println!(
                        "{} {}  [{}]",
                        row.display_icon(),
                        row.display_title(),
                        row.properties_summary()
                    );
                }
            }
            Ok(())
        }
        DbCommand::CreateItem {
            database,
            title,
            status,
            properties,
            body,
            json: as_json,
        } => {
            let extra_props = properties
                .as_deref()
                .map(serde_json::from_str::<Value>)
                .transpose()
                .context("Invalid --properties JSON")?;
            let created = op_create_database_item(
                client,
                storage,
                &database,
                &title,
                status.as_deref(),
                extra_props,
                body.as_deref(),
            )
            .await?;
            if as_json {
                print_json(&created)?;
            } else {
                println!("Created DB item '{}' ({}).", created.title, created.id);
            }
            Ok(())
        }
        DbCommand::UpdateItem {
            page_id,
            title,
            status,
            properties,
            json: as_json,
        } => {
            let extra_props = properties
                .as_deref()
                .map(serde_json::from_str::<Value>)
                .transpose()
                .context("Invalid --properties JSON")?;
            let updated = op_update_database_item(
                client,
                storage,
                &page_id,
                title.as_deref(),
                status.as_deref(),
                extra_props,
            )
            .await?;
            if as_json {
                print_json(&updated)?;
            } else {
                println!("Updated DB item '{}' ({}).", updated.title, updated.id);
            }
            Ok(())
        }
    }
}

async fn handle_comments_cmd(
    client: &NotionClient,
    storage: &Storage,
    action: CommentsCommand,
) -> Result<()> {
    match action {
        CommentsCommand::List {
            page,
            offline,
            json: as_json,
        } => {
            let comments = op_list_comments(client, storage, &page, offline).await?;
            if as_json {
                print_json(&json!({ "ok": true, "count": comments.len(), "comments": comments }))?;
            } else if comments.is_empty() {
                println!("No comments on page.");
            } else {
                for c in comments {
                    println!("[{}] {}: {}", c.created_time, c.author, c.body);
                }
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
                println!("Added comment ({}) by {}.", comment.id, comment.author);
            }
            Ok(())
        }
    }
}

async fn handle_daemon_cmd(config: AppConfig, action: DaemonCommand) -> Result<()> {
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

async fn handle_doctor_cmd(
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
    });

    if as_json {
        print_json(&report)?;
    } else {
        println!("@bitefoundry/notion v{}", env!("CARGO_PKG_VERSION"));
        println!("  Config:         {}", config::config_file_path().display());
        println!("  SQLite DB:      {} (ok: {})", db_file.display(), storage_ok);
        println!("  Workspace:      {} ({})", ws_name, ws_id);
        println!("  Authenticated:  {}", cred.is_some());
        println!("  Image Protocol: {}", caps.protocol.label());
        if let Some(probe) = &online_probe {
            println!("  Online Probe:   {probe}");
        }
    }
    Ok(())
}

// ============================================================================
// Shared Domain Operations (used by CLI, HTTP API `serve.rs`, and MCP `mcp.rs`)
// ============================================================================

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
        "workspace_name": cred
            .as_ref()
            .map(|c| c.workspace_name.clone())
            .or_else(|| user.workspace_name.clone())
            .unwrap_or_else(|| profile.clone()),
        "auth_mode": cred.as_ref().map(|c| c.auth_mode.clone()).unwrap_or_else(|| "token".into()),
        "api_base_url": config.effective_api_base_url(),
        "user": user,
    }))
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
        let mut hits = Vec::new();
        for p in resp.pages {
            hits.push(NotionSearchHit {
                id: p.id.clone(),
                workspace_id: p.workspace_id.clone(),
                item_type: "page".into(),
                title: p.title.clone(),
                icon: p.icon.clone(),
                snippet: p.properties_summary(),
                url: p.url.clone(),
                last_edited_time: p.last_edited_time.clone(),
            });
        }
        for d in resp.databases {
            hits.push(NotionSearchHit {
                id: d.id.clone(),
                workspace_id: d.workspace_id.clone(),
                item_type: "database".into(),
                title: d.title.clone(),
                icon: d.icon.clone(),
                snippet: d.description.clone(),
                url: d.url.clone(),
                last_edited_time: d.last_edited_time.clone(),
            });
        }
        hits.truncate(limit);
        return Ok(hits);
    }

    let local = storage.search_local(&ws_id, query, limit)?;
    if !local.is_empty() || offline {
        return Ok(local);
    }
    // Fallback to remote search when local cache is empty
    let resp = client
        .search_all(Some(query), SearchFilterObject::All, limit)
        .await?;
    let _ = storage.upsert_pages(&resp.pages);
    let _ = storage.upsert_databases(&resp.databases);
    let refreshed = storage.search_local(&ws_id, query, limit)?;
    if !refreshed.is_empty() {
        return Ok(refreshed);
    }
    Ok(resp
        .pages
        .into_iter()
        .map(|p| NotionSearchHit {
            id: p.id,
            workspace_id: p.workspace_id,
            item_type: "page".into(),
            title: p.title,
            icon: p.icon,
            snippet: p.plain_text,
            url: p.url,
            last_edited_time: p.last_edited_time,
        })
        .take(limit)
        .collect())
}

pub async fn op_list_pages(
    client: &NotionClient,
    storage: &Storage,
    include_archived: bool,
    refresh: bool,
    offline: bool,
) -> Result<Vec<NotionPage>> {
    let ws_id = client.auth().active_workspace_id().await;
    if !offline {
        let cached = storage.list_pages(&ws_id, include_archived)?;
        if refresh || cached.is_empty() {
            let (_, pages, _) = sync::sync_workspace_tree(client, storage).await?;
            return Ok(pages
                .into_iter()
                .filter(|p| include_archived || !p.archived)
                .collect());
        }
        return Ok(cached);
    }
    storage.list_pages(&ws_id, include_archived)
}

pub async fn op_get_page(
    client: &NotionClient,
    storage: &Storage,
    page: &str,
    offline: bool,
) -> Result<NotionPageDetail> {
    let ws_id = client.auth().active_workspace_id().await;
    let id = resolver::resolve_page_id(storage, (!offline).then_some(client), &ws_id, page).await?;
    if offline {
        return storage
            .load_page_detail(&ws_id, &id)?
            .ok_or_else(|| anyhow::anyhow!("Page '{page}' not found in local cache"));
    }
    sync::sync_page_detail(client, storage, &id).await
}

#[allow(clippy::too_many_arguments)]
pub async fn op_create_page(
    client: &NotionClient,
    storage: &Storage,
    parent: &str,
    title: &str,
    icon: Option<&str>,
    markdown_body: Option<&str>,
    parent_is_database: bool,
    extra_properties: Option<Value>,
) -> Result<NotionPage> {
    let ws_id = client.auth().active_workspace_id().await;
    let parent_id = if parent_is_database {
        resolver::resolve_database_id(storage, Some(client), &ws_id, parent).await?
    } else {
        resolver::resolve_page_id(storage, Some(client), &ws_id, parent).await?
    };

    let parent_json = if parent_is_database {
        json!({ "database_id": parent_id })
    } else {
        json!({ "page_id": parent_id })
    };

    let mut props = match extra_properties {
        Some(Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    };
    let title_key = if parent_is_database { "Name" } else { "title" };
    if !props.contains_key(title_key) && !props.contains_key("title") && !props.contains_key("Name") {
        props.insert(
            title_key.to_string(),
            json!({ "title": [{ "type": "text", "text": { "content": title } }] }),
        );
    }

    let mut payload = json!({
        "parent": parent_json,
        "properties": Value::Object(props),
    });
    if let Some(ic) = icon.map(str::trim).filter(|s| !s.is_empty()) {
        payload["icon"] = json!({ "type": "emoji", "emoji": ic });
    }
    if let Some(body) = markdown_body.map(str::trim).filter(|s| !s.is_empty()) {
        let blocks = parse_slash_command_blocks(body);
        if !blocks.is_empty() {
            payload["children"] = Value::Array(blocks);
        }
    }

    let created = client.create_page(payload).await?;
    let _ = storage.upsert_page(&created);
    Ok(created)
}

pub async fn op_update_page_meta(
    client: &NotionClient,
    storage: &Storage,
    page: &str,
    title: Option<&str>,
    icon: Option<&str>,
) -> Result<NotionPage> {
    let ws_id = client.auth().active_workspace_id().await;
    let id = resolver::resolve_page_id(storage, Some(client), &ws_id, page).await?;
    let mut payload = serde_json::Map::new();
    if let Some(t) = title {
        payload.insert(
            "properties".into(),
            json!({ "title": { "title": [{ "type": "text", "text": { "content": t } }] } }),
        );
    }
    if let Some(ic) = icon.map(str::trim).filter(|s| !s.is_empty()) {
        payload.insert("icon".into(), json!({ "type": "emoji", "emoji": ic }));
    }
    let updated = client.update_page(&id, Value::Object(payload)).await?;
    let _ = storage.upsert_page(&updated);
    Ok(updated)
}

pub async fn op_archive_page(
    client: &NotionClient,
    storage: &Storage,
    page: &str,
    archived: bool,
) -> Result<NotionPage> {
    let ws_id = client.auth().active_workspace_id().await;
    let id = resolver::resolve_page_id(storage, Some(client), &ws_id, page).await?;
    let updated = client.set_page_archived(&id, archived).await?;
    let _ = storage.upsert_page(&updated);
    Ok(updated)
}

pub async fn op_list_blocks(
    client: &NotionClient,
    storage: &Storage,
    block_or_page: &str,
    offline: bool,
) -> Result<Vec<NotionBlock>> {
    let ws_id = client.auth().active_workspace_id().await;
    let id = resolver::resolve_page_id(storage, (!offline).then_some(client), &ws_id, block_or_page)
        .await?;
    if offline {
        return storage.load_page_blocks(&ws_id, &id);
    }
    let blocks = client.fetch_block_tree(&id, None).await?;
    let _ = storage.replace_page_blocks(&ws_id, &id, &blocks);
    Ok(blocks)
}

pub async fn op_append_blocks(
    client: &NotionClient,
    storage: &Storage,
    page_or_block: &str,
    markdown_or_slash: &str,
) -> Result<Vec<NotionBlock>> {
    let ws_id = client.auth().active_workspace_id().await;
    let id = resolver::resolve_page_id(storage, Some(client), &ws_id, page_or_block).await?;
    let children = parse_slash_command_blocks(markdown_or_slash);
    if children.is_empty() {
        bail!("No blocks produced from input text");
    }
    let appended = client.append_block_children(&id, children).await?;
    if let Ok(full_tree) = client.fetch_block_tree(&id, None).await {
        let _ = storage.replace_page_blocks(&ws_id, &id, &full_tree);
    }
    Ok(appended)
}

fn is_patchable_block_type(block_type: &str) -> bool {
    matches!(
        block_type,
        "paragraph"
            | "heading_1"
            | "heading_2"
            | "heading_3"
            | "bulleted_list_item"
            | "numbered_list_item"
            | "to_do"
            | "toggle"
            | "callout"
            | "quote"
            | "code"
    )
}

fn build_block_update_payload(payload: &Value, block_type: &str) -> Option<Value> {
    if !is_patchable_block_type(block_type) {
        return None;
    }
    let inner = payload.get(block_type)?.clone();
    let mut obj = serde_json::Map::new();
    obj.insert(block_type.to_string(), inner);
    Some(Value::Object(obj))
}

fn has_real_remote_ids(blocks: &[NotionBlock]) -> bool {
    !blocks.is_empty()
        && blocks
            .iter()
            .all(|b| !b.id.trim().is_empty() && !b.id.starts_with("__optimistic_"))
}

fn is_stale_block_error(err: &anyhow::Error) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("404")
        || msg.contains("object_not_found")
        || msg.contains("archived")
        || msg.contains("could not find block")
}

async fn patch_blocks_concurrently(
    client: &NotionClient,
    page_id: &str,
    old_slice: &[NotionBlock],
    payloads_slice: &[Value],
) -> Result<Vec<NotionBlock>> {
    let jobs: Vec<(usize, String, Value, Vec<NotionBlock>, bool)> = old_slice
        .iter()
        .zip(payloads_slice.iter())
        .enumerate()
        .map(|(idx, (old_blk, payload))| {
            let update_payload =
                build_block_update_payload(payload, &old_blk.block_type).expect("patchable");
            (
                idx,
                old_blk.id.clone(),
                update_payload,
                old_blk.children.clone(),
                old_blk.has_children,
            )
        })
        .collect();

    let results: Vec<Result<(usize, NotionBlock)>> = stream::iter(jobs)
        .map(
            |(idx, blk_id, update_payload, prev_children, prev_has_children)| async move {
                let mut updated = client
                    .update_block(&blk_id, page_id, update_payload)
                    .await?;
                if updated.children.is_empty() && !prev_children.is_empty() {
                    updated.children = prev_children;
                    updated.has_children = prev_has_children || !updated.children.is_empty();
                }
                Ok((idx, updated))
            },
        )
        .buffer_unordered(4)
        .collect()
        .await;

    let mut indexed: Vec<(usize, NotionBlock)> = Vec::with_capacity(results.len());
    for res in results {
        indexed.push(res?);
    }
    indexed.sort_by_key(|(idx, _)| *idx);
    Ok(indexed.into_iter().map(|(_, blk)| blk).collect())
}

async fn delete_blocks_concurrently(
    client: &NotionClient,
    old_slice: &[NotionBlock],
) -> Result<()> {
    let del_jobs: Vec<(String, bool)> = old_slice
        .iter()
        .filter(|blk| !is_preserved_anchor_block(blk) && !blk.id.trim().is_empty())
        .map(|blk| (blk.id.clone(), blk.block_type == "unsupported"))
        .collect();
    let del_results: Vec<Result<()>> = stream::iter(del_jobs)
        .map(|(blk_id, ignore_err)| async move {
            match client.delete_block(&blk_id).await {
                Ok(_) => Ok(()),
                Err(_) if ignore_err => Ok(()),
                Err(err) => Err(err),
            }
        })
        .buffer_unordered(4)
        .collect()
        .await;
    for res in del_results {
        res?;
    }
    Ok(())
}

async fn diff_sync_editable_segment(
    client: &NotionClient,
    page_id: &str,
    editable_existing: &[NotionBlock],
    new_payloads: &[Value],
    new_parsed: &[NotionBlock],
    prev_anchor_id: Option<&str>,
    has_following_anchors: bool,
) -> Result<Vec<NotionBlock>> {
    let old_keys: Vec<(String, String)> = editable_existing
        .iter()
        .map(block_comparison_key)
        .collect();
    let new_keys: Vec<(String, String)> = new_parsed.iter().map(block_comparison_key).collect();

    let mut prefix_len = 0usize;
    while prefix_len < old_keys.len()
        && prefix_len < new_keys.len()
        && old_keys[prefix_len] == new_keys[prefix_len]
    {
        prefix_len += 1;
    }

    let mut suffix_len = 0usize;
    while suffix_len < (old_keys.len() - prefix_len)
        && suffix_len < (new_keys.len() - prefix_len)
        && old_keys[old_keys.len() - 1 - suffix_len] == new_keys[new_keys.len() - 1 - suffix_len]
    {
        suffix_len += 1;
    }

    let can_in_place_patch = |old_slice: &[NotionBlock],
                              new_slice: &[NotionBlock],
                              payloads_slice: &[Value]|
     -> bool {
        !old_slice.is_empty()
            && old_slice.len() == new_slice.len()
            && old_slice
                .iter()
                .zip(new_slice.iter())
                .zip(payloads_slice.iter())
                .all(|((o, n), p)| {
                    o.block_type == n.block_type
                        && build_block_update_payload(p, &o.block_type).is_some()
                })
    };

    // Notion's `PATCH /v1/blocks/{id}/children` supports `"after"` (not `"before"`).
    // When modifying index 0 with no preceding anchor block and a non-empty suffix, fold the
    // editable suffix if we cannot patch in place (and it isn't a pure deletion) so block order
    // stays exact — while NEVER touching preserved anchor blocks outside this segment.
    if prefix_len == 0 && prev_anchor_id.is_none() && suffix_len > 0 {
        let old_mid_end = editable_existing.len() - suffix_len;
        let new_mid_end = new_payloads.len() - suffix_len;
        let old_mid = &editable_existing[..old_mid_end];
        let new_mid_p = &new_payloads[..new_mid_end];
        let new_mid_b = &new_parsed[..new_mid_end];
        let is_noop = old_mid.is_empty() && new_mid_p.is_empty();
        let is_pure_delete = !old_mid.is_empty() && new_mid_p.is_empty();
        if !is_noop && !is_pure_delete && !can_in_place_patch(old_mid, new_mid_b, new_mid_p) {
            suffix_len = 0;
        }
    }

    let old_mid_end = editable_existing.len() - suffix_len;
    let new_mid_end = new_payloads.len() - suffix_len;
    let old_mid = &editable_existing[prefix_len..old_mid_end];
    let new_mid_payloads = &new_payloads[prefix_len..new_mid_end];
    let new_mid_parsed = &new_parsed[prefix_len..new_mid_end];

    let after_id = if prefix_len > 0 {
        Some(editable_existing[prefix_len - 1].id.as_str())
    } else {
        prev_anchor_id
    };

    let updated_mid: Vec<NotionBlock> = if old_mid.is_empty() && new_mid_payloads.is_empty() {
        Vec::new()
    } else if can_in_place_patch(old_mid, new_mid_parsed, new_mid_payloads) {
        patch_blocks_concurrently(client, page_id, old_mid, new_mid_payloads).await?
    } else if old_mid.is_empty() {
        client
            .append_block_children_after(page_id, after_id, new_mid_payloads.to_vec())
            .await?
    } else if new_mid_payloads.is_empty() {
        delete_blocks_concurrently(client, old_mid).await?;
        Vec::new()
    } else if after_id.is_none() && has_following_anchors {
        // Modifying blocks before a preserved anchor at index 0 with no preceding block:
        // First patch any leading same-type blocks in place so their real ID acts as `after_id`,
        // keeping everything positioned strictly BEFORE the preserved anchor without deleting it!
        let lead_patch = old_mid
            .iter()
            .zip(new_mid_parsed.iter())
            .zip(new_mid_payloads.iter())
            .take_while(|((o, n), p)| {
                o.block_type == n.block_type
                    && build_block_update_payload(p, &o.block_type).is_some()
            })
            .count();

        if lead_patch > 0 {
            let mut out = patch_blocks_concurrently(
                client,
                page_id,
                &old_mid[..lead_patch],
                &new_mid_payloads[..lead_patch],
            )
            .await?;
            let anchor_after = out.last().map(|b| b.id.clone());
            if lead_patch < old_mid.len() {
                delete_blocks_concurrently(client, &old_mid[lead_patch..]).await?;
            }
            if lead_patch < new_mid_payloads.len() {
                let appended = client
                    .append_block_children_after(
                        page_id,
                        anchor_after.as_deref(),
                        new_mid_payloads[lead_patch..].to_vec(),
                    )
                    .await?;
                out.extend(appended);
            }
            out
        } else {
            // Different block type at index 0 before a preserved anchor:
            // Append new blocks `after: old_mid[0]` (placing them strictly BEFORE the anchor),
            // then delete `old_mid` — exact position preserved with 0 deletes on the anchor!
            let temp_after = old_mid[0].id.clone();
            let appended = client
                .append_block_children_after(
                    page_id,
                    Some(&temp_after),
                    new_mid_payloads.to_vec(),
                )
                .await?;
            delete_blocks_concurrently(client, old_mid).await?;
            appended
        }
    } else {
        delete_blocks_concurrently(client, old_mid).await?;
        client
            .append_block_children_after(page_id, after_id, new_mid_payloads.to_vec())
            .await?
    };

    let mut seg_result =
        Vec::with_capacity(prefix_len + updated_mid.len() + suffix_len);
    seg_result.extend_from_slice(&editable_existing[..prefix_len]);
    seg_result.extend(updated_mid);
    seg_result.extend_from_slice(&editable_existing[old_mid_end..]);
    Ok(seg_result)
}

async fn diff_sync_with_existing(
    client: &NotionClient,
    page_id: &str,
    existing: Vec<NotionBlock>,
    edited_markdown: &str,
) -> Result<Vec<NotionBlock>> {
    let cleaned_existing: Vec<NotionBlock> = existing
        .into_iter()
        .filter(|blk| is_preserved_anchor_block(blk) || !blk.id.trim().is_empty())
        .collect();

    let new_payloads = markdown_to_notion_blocks(edited_markdown);
    let new_parsed: Vec<NotionBlock> = new_payloads
        .iter()
        .map(|val| parse_notion_block(val, page_id, None))
        .collect();

    let (old_segments, anchors, new_ranges) =
        partition_blocks_around_anchors(&cleaned_existing, &new_parsed);

    let mut final_blocks: Vec<NotionBlock> =
        Vec::with_capacity(new_parsed.len() + anchors.len());
    let mut prev_anchor_id: Option<String> = None;

    for (seg_idx, (old_seg, range)) in old_segments
        .into_iter()
        .zip(new_ranges.into_iter())
        .enumerate()
    {
        let has_following_anchors = seg_idx < anchors.len();
        let seg_result = diff_sync_editable_segment(
            client,
            page_id,
            &old_seg,
            &new_payloads[range.clone()],
            &new_parsed[range],
            prev_anchor_id.as_deref(),
            has_following_anchors,
        )
        .await?;
        final_blocks.extend(seg_result);

        if let Some(anchor) = anchors.get(seg_idx) {
            if !anchor.id.trim().is_empty() {
                prev_anchor_id = Some(anchor.id.clone());
            }
            final_blocks.push(anchor.clone());
        }
    }

    Ok(final_blocks)
}

/// Replaces the editable content blocks of a Notion page with blocks parsed from `edited_markdown`
/// using fast diff-based sync (preserving hosted media, child pages, child databases, and structural
/// blocks in place, skipping untouched prefix/suffix blocks, patching same-type edits concurrently,
/// and reconstructing the updated `NotionPageDetail` + SQLite cache without slow full-tree re-fetches).
pub async fn op_replace_page_markdown(
    client: &NotionClient,
    storage: &Storage,
    page: &str,
    edited_markdown: &str,
) -> Result<NotionPageDetail> {
    op_diff_sync_page_markdown(client, storage, page, edited_markdown, None).await
}

/// Smart diff-based Markdown -> Notion block sync.
///
/// When `known_existing` contains the page's current top-level blocks with real Notion IDs,
/// skips the initial `GET /v1/blocks/{id}/children` roundtrip. If a stale/archived block ID
/// causes a 404/archived API error, automatically refetches top-level children once and retries.
pub async fn op_diff_sync_page_markdown(
    client: &NotionClient,
    storage: &Storage,
    page: &str,
    edited_markdown: &str,
    known_existing: Option<Vec<NotionBlock>>,
) -> Result<NotionPageDetail> {
    let ws_id = client.auth().active_workspace_id().await;
    let id = resolver::resolve_page_id(storage, Some(client), &ws_id, page).await?;

    let used_known = known_existing
        .as_ref()
        .is_some_and(|blocks| has_real_remote_ids(blocks));
    let existing = match known_existing {
        Some(blocks) if has_real_remote_ids(&blocks) => blocks,
        _ => client.list_block_children(&id, &id, None).await?,
    };

    let mut final_blocks = match diff_sync_with_existing(client, &id, existing, edited_markdown).await {
        Ok(blocks) => blocks,
        Err(err) if used_known && is_stale_block_error(&err) => {
            let fresh = client.list_block_children(&id, &id, None).await?;
            diff_sync_with_existing(client, &id, fresh, edited_markdown).await?
        }
        Err(err) => return Err(err),
    };

    // Only fetch children for blocks that actually report nested children not already in memory.
    for blk in &mut final_blocks {
        if blk.has_children
            && blk.children.is_empty()
            && !matches!(
                blk.block_type.as_str(),
                "child_page" | "child_database" | "unsupported"
            )
        {
            if let Ok(children) = client.fetch_block_tree(&blk.id, Some(3)).await {
                blk.children = children;
            }
        }
    }

    let mut page_meta = match storage.get_page(&ws_id, &id)? {
        Some(p) => p,
        None => {
            let fetched = client.get_page(&id).await?;
            let _ = storage.upsert_page(&fetched);
            fetched
        }
    };
    let comments = storage.load_page_comments(&ws_id, &id).unwrap_or_default();

    let _ = storage.replace_page_blocks(&ws_id, &id, &final_blocks);
    page_meta.plain_text = blocks_to_plain_text(&final_blocks);
    let markdown = blocks_to_markdown(&final_blocks);
    let media = collect_media_refs(&final_blocks);

    Ok(NotionPageDetail {
        page: page_meta,
        blocks: final_blocks,
        comments,
        markdown,
        media,
    })
}

pub async fn op_update_block_text(
    client: &NotionClient,
    block_id: &str,
    block_type: Option<&str>,
    text: &str,
    checked: Option<bool>,
) -> Result<NotionBlock> {
    let btype = match block_type {
        Some(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => client.get_block(block_id, "").await?.block_type,
    };
    let mut inner = serde_json::Map::new();
    inner.insert(
        "rich_text".into(),
        Value::Array(parse_inline_markdown_rich_text(text)),
    );
    if btype == "to_do" {
        if let Some(c) = checked {
            inner.insert("checked".into(), json!(c));
        }
    }
    let mut payload = serde_json::Map::new();
    payload.insert(btype, Value::Object(inner));
    client
        .update_block(block_id, "", Value::Object(payload))
        .await
}

pub async fn op_toggle_todo(
    client: &NotionClient,
    _storage: &Storage,
    block_id: &str,
    explicit_checked: Option<bool>,
) -> Result<NotionBlock> {
    let target_checked = match explicit_checked {
        Some(val) => val,
        None => {
            let current = client.get_block(block_id, "").await?;
            match current.kind {
                NotionBlockKind::ToDo { checked, .. } => !checked,
                _ => bail!("Block '{block_id}' is not a to_do block"),
            }
        }
    };
    client.set_todo_checked(block_id, "", target_checked).await
}

pub async fn op_list_databases(
    client: &NotionClient,
    storage: &Storage,
    refresh: bool,
    offline: bool,
) -> Result<Vec<NotionDatabase>> {
    let ws_id = client.auth().active_workspace_id().await;
    if !offline {
        let cached = storage.list_databases(&ws_id)?;
        if refresh || cached.is_empty() {
            let (_, _, dbs) = sync::sync_workspace_tree(client, storage).await?;
            return Ok(dbs);
        }
        return Ok(cached);
    }
    storage.list_databases(&ws_id)
}

pub async fn op_get_database(
    client: &NotionClient,
    storage: &Storage,
    database: &str,
    offline: bool,
) -> Result<NotionDatabase> {
    let ws_id = client.auth().active_workspace_id().await;
    let id =
        resolver::resolve_database_id(storage, (!offline).then_some(client), &ws_id, database)
            .await?;
    if offline {
        return storage
            .get_database(&ws_id, &id)?
            .ok_or_else(|| anyhow::anyhow!("Database '{database}' not found in local cache"));
    }
    let db = client.get_database(&id).await?;
    let _ = storage.upsert_database(&db);
    Ok(db)
}

pub async fn op_query_database(
    client: &NotionClient,
    storage: &Storage,
    database: &str,
    filter: Option<Value>,
    sorts: Option<Value>,
    limit: usize,
    offline: bool,
) -> Result<Vec<NotionPage>> {
    let ws_id = client.auth().active_workspace_id().await;
    let id =
        resolver::resolve_database_id(storage, (!offline).then_some(client), &ws_id, database)
            .await?;
    if offline {
        let mut rows = storage.list_database_rows(&ws_id, &id)?;
        rows.truncate(limit);
        return Ok(rows);
    }
    let rows = client.query_database_all(&id, filter, sorts, limit).await?;
    let _ = storage.upsert_pages(&rows);
    Ok(rows)
}

pub async fn op_create_database_item(
    client: &NotionClient,
    storage: &Storage,
    database: &str,
    title: &str,
    status: Option<&str>,
    extra_properties: Option<Value>,
    body: Option<&str>,
) -> Result<NotionPage> {
    let ws_id = client.auth().active_workspace_id().await;
    let db_id = resolver::resolve_database_id(storage, Some(client), &ws_id, database).await?;
    let db = storage
        .get_database(&ws_id, &db_id)?
        .or_else(|| None);
    let title_prop_name = db
        .as_ref()
        .and_then(|d| {
            d.schema
                .iter()
                .find(|(_, s)| s.prop_type == "title")
                .map(|(k, _)| k.clone())
        })
        .unwrap_or_else(|| "Name".to_string());

    let mut props = match extra_properties {
        Some(Value::Object(m)) => m,
        _ => serde_json::Map::new(),
    };
    props.insert(
        title_prop_name,
        json!({ "title": [{ "type": "text", "text": { "content": title } }] }),
    );
    if let Some(st) = status.map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(group_prop) = db.as_ref().and_then(|d| d.board_group_property()) {
            if group_prop.prop_type == "status" {
                props.insert(group_prop.name.clone(), json!({ "status": { "name": st } }));
            } else {
                props.insert(group_prop.name.clone(), json!({ "select": { "name": st } }));
            }
        } else {
            props.insert("Status".into(), json!({ "status": { "name": st } }));
        }
    }

    op_create_page(
        client,
        storage,
        &db_id,
        title,
        None,
        body,
        true,
        Some(Value::Object(props)),
    )
    .await
}

pub async fn op_update_database_item(
    client: &NotionClient,
    storage: &Storage,
    page_id: &str,
    title: Option<&str>,
    status: Option<&str>,
    extra_properties: Option<Value>,
) -> Result<NotionPage> {
    let id = normalize_notion_id(page_id);
    let mut props = match extra_properties {
        Some(Value::Object(m)) => m,
        _ => serde_json::Map::new(),
    };
    if let Some(t) = title {
        props.insert(
            "Name".into(),
            json!({ "title": [{ "type": "text", "text": { "content": t } }] }),
        );
    }
    if let Some(st) = status.map(str::trim).filter(|s| !s.is_empty()) {
        props.insert("Status".into(), json!({ "status": { "name": st } }));
    }
    let updated = client
        .update_page(&id, json!({ "properties": Value::Object(props) }))
        .await?;
    let _ = storage.upsert_page(&updated);
    Ok(updated)
}

pub async fn op_list_comments(
    client: &NotionClient,
    storage: &Storage,
    page: &str,
    offline: bool,
) -> Result<Vec<NotionComment>> {
    let ws_id = client.auth().active_workspace_id().await;
    let id = resolver::resolve_page_id(storage, (!offline).then_some(client), &ws_id, page).await?;
    if offline {
        return storage.load_page_comments(&ws_id, &id);
    }
    let comments = client.list_comments(&id).await?;
    let _ = storage.replace_page_comments(&ws_id, &id, &comments);
    Ok(comments)
}

pub async fn op_add_comment(
    client: &NotionClient,
    storage: &Storage,
    page: &str,
    discussion_id: Option<&str>,
    text: &str,
) -> Result<NotionComment> {
    let ws_id = client.auth().active_workspace_id().await;
    let id = resolver::resolve_page_id(storage, Some(client), &ws_id, page).await?;
    let comment = client.add_comment(Some(&id), discussion_id, text).await?;
    if let Ok(all) = client.list_comments(&id).await {
        let _ = storage.replace_page_comments(&ws_id, &id, &all);
    }
    Ok(comment)
}

fn read_body_or_file(body: Option<String>, file: Option<PathBuf>) -> Result<Option<String>> {
    if let Some(path) = file {
        let content = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read input file: {}", path.display()))?;
        return Ok(Some(content));
    }
    Ok(body)
}

fn print_json<T: serde::Serialize>(val: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(val)?);
    Ok(())
}
