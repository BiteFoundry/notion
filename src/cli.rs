//! Standalone `@bitefoundry/notion` (`notion`) CLI parser, command dispatcher,
//! and shared domain operations used across CLI, HTTP API (`serve`), and MCP (`mcp`).

use std::{
    collections::HashSet,
    ffi::OsString,
    fs,
    io::{self, IsTerminal, Write},
    path::PathBuf,
};

use anyhow::{bail, Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use serde_json::{json, Value};

use crate::{
    auth::{AuthManager, OAuthLoginOptions},
    config::{self, AppConfig, WorkspaceProfile, DEFAULT_SERVE_PORT},
    daemon, img_preview,
    models::{
        normalize_notion_id, NotionBlock, NotionBlockKind, NotionComment, NotionDatabase,
        NotionPage, NotionPageDetail, NotionPropertyValue, NotionSearchHit,
    },
    notion::{media, NotionClient, SearchFilterObject},
    parser::{
        blocks_to_markdown, notion::rich_text_to_markdown, parse_notion_block, parse_parent, parse_rich_text_array,
        parse_slash_command_blocks,
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
        refresh: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
        /// Return only the page ID, title, last edited time, and Markdown
        #[arg(long, requires = "json")]
        compact: bool,
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
    /// Edit the inline Markdown text of one block
    Edit {
        block_id: String,
        #[arg(long)]
        text: String,
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
    /// Run stdio JSON-RPC 2.0 MCP server
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

pub fn parse_cli_from<I, T>(args: I) -> std::result::Result<Cli, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let raw: Vec<OsString> = args.into_iter().map(Into::into).collect();
    let normalized = normalize_cli_args(raw);
    Cli::try_parse_from(normalized)
}

fn affected_text_spec(path: &[String]) -> Option<(&'static [&'static str], Option<usize>)> {
    match path {
        [a, b] if a == "pages" && b == "create" => Some((&["title", "body"], None)),
        [a, b] if a == "pages" && b == "append" => Some((&[], Some(1))),
        [a, b] if a == "pages" && b == "edit" => Some((&["title"], None)),
        [a, b] if a == "blocks" && b == "append" => Some((&[], Some(1))),
        [a, b] if a == "blocks" && b == "edit" => Some((&["text"], None)),
        [a, b] if a == "db" && b == "create-item" => Some((&["title", "body"], None)),
        [a, b] if a == "db" && b == "update-item" => Some((&["title"], None)),
        [a, b] if a == "comments" && b == "add" => Some((&[], Some(1))),
        [a] if a == "search" => Some((&[], Some(0))),
        _ => None,
    }
}

fn find_long_option<'a>(cmd: &'a clap::Command, name: &str) -> Option<&'a clap::Arg> {
    cmd.get_arguments().find(|arg| arg.get_long() == Some(name))
}

fn find_short_option<'a>(cmd: &'a clap::Command, ch: char) -> Option<&'a clap::Arg> {
    cmd.get_arguments().find(|arg| arg.get_short() == Some(ch))
}

fn could_be_hyphen_literal(s: &str) -> bool {
    if !s.starts_with('-') || s == "--" {
        return false;
    }
    if s == "-" {
        return true;
    }
    if let Some(rest) = s.strip_prefix("--") {
        // `--dry-run`, `--json`, `--help`, etc. start with ASCII alphanumeric after `--`.
        // Markdown dividers (`---`) or `-- ` / non-ASCII text are treated as literals.
        return !rest
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric);
    }
    let rest = &s[1..];
    // Preserve single-letter ASCII short flags (`-h`, `-V`, `-w`, `-x`, ...).
    !(rest.len() == 1 && rest.as_bytes()[0].is_ascii_alphabetic())
}

fn is_separate_hyphen_literal(s: &str, cmd: &clap::Command) -> bool {
    if !could_be_hyphen_literal(s) {
        return false;
    }
    if s == "-" || s.starts_with("--") {
        return true;
    }
    let rest = &s[1..];
    if let Some(first) = rest.chars().next() {
        if find_short_option(cmd, first)
            .is_some_and(|arg| arg.get_action().takes_values())
        {
            return false;
        }
        if rest.chars().all(|c| {
            find_short_option(cmd, c).is_some_and(|arg| !arg.get_action().takes_values())
        }) {
            return false;
        }
    }
    true
}

fn is_flag_like_token(arg: &OsString, cmd: &clap::Command) -> bool {
    arg.to_str()
        .is_some_and(|s| s.starts_with('-') && !is_separate_hyphen_literal(s, cmd))
}

fn is_long_equals_option(s: &str) -> bool {
    s.strip_prefix("--")
        .and_then(|rest| rest.split_once('='))
        .is_some_and(|(name, _)| {
            name.as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
        })
}

fn normalize_cli_args(raw: Vec<OsString>) -> Vec<OsString> {
    if !raw.iter().skip(1).take_while(|arg| *arg != "--").any(|arg| {
        arg.to_str().is_some_and(could_be_hyphen_literal)
    }) {
        return raw;
    }
    let mut root = Cli::command();
    root.build();

    let mut cmd: &clap::Command = &root;
    let mut subcmd_path: Vec<String> = Vec::new();
    let mut idx = 1usize;

    while idx < raw.len() && cmd.has_subcommands() {
        let Some(s) = raw[idx].to_str() else {
            break;
        };
        if s == "--" {
            break;
        }
        if s.starts_with('-') {
            if let Some(name) = s.strip_prefix("--") {
                if !name.contains('=')
                    && find_long_option(cmd, name)
                        .is_some_and(|arg| arg.get_action().takes_values())
                {
                    idx += 1;
                    if idx < raw.len()
                        && !raw[idx].to_str().is_some_and(|next| next.starts_with('-'))
                    {
                        idx += 1;
                    }
                    continue;
                }
            } else if s.len() == 2 {
                let ch = s.chars().nth(1).unwrap();
                if find_short_option(cmd, ch)
                    .is_some_and(|arg| arg.get_action().takes_values())
                {
                    idx += 1;
                    if idx < raw.len()
                        && !raw[idx].to_str().is_some_and(|next| next.starts_with('-'))
                    {
                        idx += 1;
                    }
                    continue;
                }
            }
            idx += 1;
            continue;
        }

        if let Some(sub) = cmd
            .get_subcommands()
            .find(|sub| sub.get_name() == s || sub.get_all_aliases().any(|alias| alias == s))
        {
            subcmd_path.push(sub.get_name().to_owned());
            cmd = sub;
            idx += 1;
        } else {
            break;
        }
    }

    let Some((affected_opts, affected_pos)) = affected_text_spec(&subcmd_path) else {
        return raw;
    };

    let prefix = &raw[..idx];
    let tail = &raw[idx..];
    let mut in_place: Vec<OsString> = Vec::with_capacity(tail.len());
    let mut option_tokens: Vec<OsString> = Vec::new();
    let mut positional_tokens: Vec<OsString> = Vec::new();
    let mut trailing_after_dd: Vec<OsString> = Vec::new();
    let mut saw_hyphen_positional = false;
    let mut pos_idx = 0usize;
    let mut i = 0usize;

    while i < tail.len() {
        let tok = &tail[i];
        let s_opt = tok.to_str();

        if s_opt == Some("--") {
            in_place.extend(tail[i..].iter().cloned());
            trailing_after_dd.extend(tail[i + 1..].iter().cloned());
            break;
        }

        if let Some(s) = s_opt {
            if is_long_equals_option(s) {
                in_place.push(tok.clone());
                option_tokens.push(tok.clone());
                i += 1;
                continue;
            }

            if let Some(name) = s.strip_prefix("--") {
                if name
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                {
                    let takes_value = find_long_option(cmd, name)
                        .is_some_and(|arg| arg.get_action().takes_values());
                    if takes_value {
                        if affected_opts.contains(&name) {
                            if let Some(next) = tail.get(i + 1) {
                                if next.to_str().is_some_and(|ns| {
                                    ns.starts_with('-') && is_separate_hyphen_literal(ns, cmd)
                                }) {
                                    let mut combined = OsString::from(format!("--{name}="));
                                    combined.push(next);
                                    in_place.push(combined.clone());
                                    option_tokens.push(combined);
                                    i += 2;
                                    continue;
                                }
                                if !next.to_str().is_some_and(|ns| ns.starts_with('-')) {
                                    in_place.push(tok.clone());
                                    in_place.push(next.clone());
                                    option_tokens.push(tok.clone());
                                    option_tokens.push(next.clone());
                                    i += 2;
                                    continue;
                                }
                            }
                            in_place.push(tok.clone());
                            option_tokens.push(tok.clone());
                            i += 1;
                            continue;
                        } else {
                            if let Some(next) = tail.get(i + 1) {
                                if !is_flag_like_token(next, cmd) && next.to_str() != Some("--") {
                                    in_place.push(tok.clone());
                                    in_place.push(next.clone());
                                    option_tokens.push(tok.clone());
                                    option_tokens.push(next.clone());
                                    i += 2;
                                    continue;
                                }
                            }
                            in_place.push(tok.clone());
                            option_tokens.push(tok.clone());
                            i += 1;
                            continue;
                        }
                    }

                    in_place.push(tok.clone());
                    option_tokens.push(tok.clone());
                    i += 1;
                    continue;
                }
            }

            if s.starts_with('-') && !is_separate_hyphen_literal(s, cmd) {
                if s.len() == 2 {
                    let ch = s.chars().nth(1).unwrap();
                    if find_short_option(cmd, ch)
                        .is_some_and(|arg| arg.get_action().takes_values())
                    {
                        if let Some(next) = tail.get(i + 1) {
                            if !is_flag_like_token(next, cmd) && next.to_str() != Some("--") {
                                in_place.push(tok.clone());
                                in_place.push(next.clone());
                                option_tokens.push(tok.clone());
                                option_tokens.push(next.clone());
                                i += 2;
                                continue;
                            }
                        }
                    }
                }
                in_place.push(tok.clone());
                option_tokens.push(tok.clone());
                i += 1;
                continue;
            }
        }

        if s_opt.is_some_and(|s| s.starts_with('-')) {
            if affected_pos == Some(pos_idx) {
                saw_hyphen_positional = true;
                in_place.push(tok.clone());
                positional_tokens.push(tok.clone());
                pos_idx += 1;
                i += 1;
            } else {
                in_place.push(tok.clone());
                option_tokens.push(tok.clone());
                i += 1;
            }
        } else {
            in_place.push(tok.clone());
            positional_tokens.push(tok.clone());
            pos_idx += 1;
            i += 1;
        }
    }

    let mut out = Vec::with_capacity(raw.len() + usize::from(saw_hyphen_positional));
    out.extend(prefix.iter().cloned());
    if saw_hyphen_positional {
        out.extend(option_tokens);
        out.push(OsString::from("--"));
        out.extend(positional_tokens);
        out.extend(trailing_after_dd);
    } else {
        out.extend(in_place);
    }
    out
}

pub async fn run_from_env() -> Result<()> {
    let cli = parse_cli_from(std::env::args_os()).unwrap_or_else(|err| err.exit());
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
            refresh,
            offline,
            json: as_json,
            compact,
        } => {
            let detail = op_get_page_with_options(client, storage, &page, refresh, offline).await?;
            if compact {
                print_json(&detail.compact())?;
            } else if as_json {
                print_json(&detail)?;
            } else {
                if let Some(error) = &detail.comments_error {
                    eprintln!("Could not load comments: {error}");
                }
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
            let edited_page = if editor {
                let detail = sync::sync_page_detail_force(client, storage, &page_id).await?;
                let initial = crate::page_edit::editor_markdown_for_detail(&detail);
                let edited = crate::ui::composer::external_edit(&initial)
                    .context("Failed to edit page in external $EDITOR")?;
                if edited.trim_end() == initial.trim_end() {
                    Some(detail.page)
                } else {
                    let save_result: Result<NotionPage> = async {
                        let _ = crate::page_edit::PageEdit::plan(&page_id, &detail.blocks, &edited)?;
                        let fresh = client.fetch_block_tree(&page_id, None).await?;
                        if !crate::page_edit::same_edit_baseline(&detail.blocks, &fresh) {
                            bail!("Page blocks changed while the editor was open. Reopen the page before saving.");
                        }
                        Ok(apply_page_edit(client, storage, &page_id, &edited, Some(fresh), false)
                            .await?
                            .page)
                    }
                    .await;
                    match save_result {
                        Ok(page) => Some(page),
                        Err(err) => match crate::page_edit::save_edit_recovery(&page_id, &edited) {
                            Ok(path) => {
                                bail!(
                                    "{err}\nEdited text saved for recovery at: {}",
                                    path.display()
                                );
                            }
                            Err(_) => return Err(err),
                        },
                    }
                }
            } else {
                None
            };
            let updated = if title.is_some() || icon.is_some() {
                op_update_page_meta(client, storage, &page_id, title.as_deref(), icon.as_deref())
                    .await?
            } else if let Some(page) = edited_page {
                page
            } else {
                sync::sync_page_detail_force(client, storage, &page_id).await?.page
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
        BlocksCommand::Edit { block_id, text, json: as_json } => {
            let block = op_update_block_text(client, storage, &block_id, None, &text, None).await?;
            if as_json {
                print_json(&block)?;
            } else {
                println!("Updated block {}.", block.id);
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
            let resp = op_delete_block(client, storage, &block_id).await?;
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
    op_get_page_with_options(client, storage, page, false, offline).await
}

pub async fn op_get_page_with_options(
    client: &NotionClient,
    storage: &Storage,
    page: &str,
    refresh: bool,
    offline: bool,
) -> Result<NotionPageDetail> {
    let ws_id = client.auth().active_workspace_id().await;
    let id = resolver::resolve_page_id(storage, (!offline).then_some(client), &ws_id, page).await?;
    if offline {
        return storage
            .load_page_detail(&ws_id, &id)?
            .ok_or_else(|| anyhow::anyhow!("Page '{page}' not found in local cache"));
    }
    if refresh {
        sync::sync_page_detail_force(client, storage, &id).await
    } else {
        sync::sync_page_detail(client, storage, &id).await
    }
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
    if !props.contains_key(title_key) && !props.values().any(|value| value.get("title").is_some()) {
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
    let title = title.map(|text| ("title", Some(text)));
    let icon = icon.map(str::trim).filter(|s| !s.is_empty()).map(Some);
    patch_page_metadata(client, storage, &id, title, icon).await
}

/// `None` leaves the field untouched. `Some(None)` clears it.
pub(crate) async fn op_update_page_metadata(
    client: &NotionClient,
    storage: &Storage,
    page: &str,
    title: Option<Option<&str>>,
    icon: Option<Option<&str>>,
) -> Result<NotionPage> {
    if title.is_none() && icon.is_none() {
        bail!("Provide title or icon");
    }
    let ws_id = client.auth().active_workspace_id().await;
    let id = resolver::resolve_page_id(storage, Some(client), &ws_id, page).await?;
    let title_key = if title.is_some() {
        let current = client.get_page(&id).await?;
        current.properties.into_iter().find_map(|(name, property)| {
            matches!(property, NotionPropertyValue::Title(_)).then_some(name)
        }).context("Page has no accessible title property")?
    } else {
        String::new()
    };
    let title = title.map(|text| (title_key.as_str(), text));
    patch_page_metadata(client, storage, &id, title, icon).await
}

async fn patch_page_metadata(
    client: &NotionClient,
    storage: &Storage,
    id: &str,
    title: Option<(&str, Option<&str>)>,
    icon: Option<Option<&str>>,
) -> Result<NotionPage> {
    let mut payload = serde_json::Map::new();
    if let Some((key, text)) = title {
        let spans = match text.filter(|text| !text.is_empty()) {
            Some(text) => json!([{ "type": "text", "text": { "content": text } }]),
            None => json!([]),
        };
        payload.insert("properties".into(), json!({ (key): { "title": spans } }));
    }
    if let Some(icon) = icon {
        let value = match icon {
            Some(emoji) => json!({ "type": "emoji", "emoji": emoji }),
            None => Value::Null,
        };
        payload.insert("icon".into(), value);
    }
    let updated = client.update_page(id, Value::Object(payload)).await?;
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
        return storage.load_cached_children(&ws_id, &id)?.ok_or_else(|| {
            anyhow::anyhow!("Children of '{block_or_page}' not found in offline cache (not fetched)")
        });
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
    let appended = client.append_block_children(&id, children).await;
    if appended.is_err() {
        let _ = storage.invalidate_page_detail(&ws_id, &id);
    }
    if let Ok(full_tree) = client.fetch_block_tree(&id, None).await {
        let _ = storage.replace_page_blocks(&ws_id, &id, &full_tree);
    }
    appended
}

/// Apply a Markdown edit using the hierarchy and rendered ranges of the original blocks.
pub async fn op_replace_page_markdown(
    client: &NotionClient,
    storage: &Storage,
    page: &str,
    edited_markdown: &str,
) -> Result<NotionPageDetail> {
    op_diff_sync_page_markdown(client, storage, page, edited_markdown, None).await
}

pub async fn op_diff_sync_page_markdown(
    client: &NotionClient,
    storage: &Storage,
    page: &str,
    edited_markdown: &str,
    known_existing: Option<Vec<NotionBlock>>,
) -> Result<NotionPageDetail> {
    apply_page_edit(client, storage, page, edited_markdown, known_existing, true).await
}

async fn apply_page_edit(
    client: &NotionClient,
    storage: &Storage,
    page: &str,
    edited_markdown: &str,
    known_existing: Option<Vec<NotionBlock>>,
    retry_stale: bool,
) -> Result<NotionPageDetail> {
    let ws_id = client.auth().active_workspace_id().await;
    let id = resolver::resolve_page_id(storage, Some(client), &ws_id, page).await?;
    let known = known_existing.filter(|blocks| blocks.iter().all(|b| {
        !b.id.trim().is_empty() && !b.id.starts_with("__optimistic_")
    }));
    let existing = match known.as_ref() {
        Some(blocks) => blocks.clone(),
        None => client.fetch_block_tree(&id, None).await?,
    };
    let plan = crate::page_edit::PageEdit::plan(&id, &existing, edited_markdown)?;
    if plan.is_empty() {
        return sync::sync_page_detail_force(client, storage, &id).await;
    }
    let column_guarded = crate::parser::has_column_blocks(&existing)
        || crate::page_edit::contains_column_marker(edited_markdown);
    if retry_stale && known.is_some() && column_guarded {
        let fresh = client.fetch_block_tree(&id, None).await?;
        if !crate::page_edit::same_edit_baseline(&existing, &fresh) {
            bail!("Page blocks changed while editing. Reload the page before saving.");
        }
    }
    if let Err(error) = plan.apply(client, &id).await {
        // Refresh the cache even on a mid-save failure. A subsequent save can match
        // already appended content at its actual position and reuse its server ID.
        let _ = storage.invalidate_page_detail(&ws_id, &id);
        let recovered = sync::sync_page_detail_force(client, storage, &id).await;
        if let Ok(fresh) = recovered {
            if retry_stale
                && !column_guarded
                && known.is_some()
                && NotionClient::is_authoritative_not_found(&error)
            {
                let retry = crate::page_edit::PageEdit::plan(&id, &fresh.blocks, edited_markdown)?;
                if let Err(error) = retry.apply(client, &id).await {
                    let _ = storage.invalidate_page_detail(&ws_id, &id);
                    let _ = sync::sync_page_detail_force(client, storage, &id).await;
                    return Err(error);
                }
                return sync::sync_page_detail_force(client, storage, &id).await;
            }
        } else if let Ok(blocks) = client.fetch_block_tree(&id, None).await {
            let _ = storage.replace_page_blocks(&ws_id, &id, &blocks);
        }
        return Err(error);
    }
    let _ = storage.invalidate_page_detail(&ws_id, &id);
    sync::sync_page_detail_force(client, storage, &id).await
}

pub async fn op_update_block_text(
    client: &NotionClient,
    storage: &Storage,
    block_id: &str,
    block_type: Option<&str>,
    text: &str,
    checked: Option<bool>,
) -> Result<NotionBlock> {
    let ws_id = client.auth().active_workspace_id().await;
    let norm_id = normalize_notion_id(block_id);
    let original = client.get_block_json(&norm_id).await?;
    let current = parse_notion_block(&original, "", None);
    let btype = &current.block_type;
    if let Some(requested) = block_type.map(str::trim).filter(|t| !t.is_empty()) {
        if requested != btype {
            bail!("Block '{block_id}' is a {btype}, not a {requested}");
        }
    }
    if !crate::page_edit::patchable(btype) {
        bail!("Block '{block_id}' does not have editable rich text");
    }
    let mut inner = serde_json::Map::new();
    let raw = original[btype]["rich_text"].as_array().map(Vec::as_slice).unwrap_or_default();
    let parsed = parse_rich_text_array(original[btype].get("rich_text"));
    if rich_text_to_markdown(&parsed) != text {
        inner.insert("rich_text".into(), json!(crate::page_edit::edited_rich_text(raw, text)?));
    }
    if let (NotionBlockKind::ToDo { checked: actual, .. }, Some(checked)) = (&current.kind, checked) {
        if *actual != checked {
            inner.insert("checked".into(), json!(checked));
        }
    }
    if inner.is_empty() {
        return Ok(current);
    }
    let mut payload = serde_json::Map::new();
    payload.insert(btype.clone(), Value::Object(inner));
    let mut updated = client
        .update_block(&norm_id, "", Value::Object(payload))
        .await?;
    if let Ok(Some((pid, _))) = storage.apply_updated_block(&ws_id, &updated) {
        if updated.page_id.trim().is_empty() {
            updated.page_id = pid;
        }
    }
    Ok(updated)
}

pub async fn op_toggle_todo(
    client: &NotionClient,
    storage: &Storage,
    block_id: &str,
    explicit_checked: Option<bool>,
) -> Result<NotionBlock> {
    let ws_id = client.auth().active_workspace_id().await;
    let norm_id = normalize_notion_id(block_id);
    let target_checked = match explicit_checked {
        Some(val) => val,
        None => {
            let current = client.get_block(&norm_id, "").await?;
            match current.kind {
                NotionBlockKind::ToDo { checked, .. } => !checked,
                _ => bail!("Block '{block_id}' is not a to_do block"),
            }
        }
    };
    let mut updated = client.set_todo_checked(&norm_id, "", target_checked).await?;
    if let Ok(Some((pid, _))) = storage.apply_updated_block(&ws_id, &updated) {
        if updated.page_id.trim().is_empty() {
            updated.page_id = pid;
        }
    }
    Ok(updated)
}

pub async fn op_delete_block(
    client: &NotionClient,
    storage: &Storage,
    block_id: &str,
) -> Result<Value> {
    let ws_id = client.auth().active_workspace_id().await;
    let norm_id = normalize_notion_id(block_id);
    let resp = client.delete_block(&norm_id).await?;
    let deleted_page = storage.delete_cached_block(&ws_id, &norm_id).ok().flatten();
    if deleted_page.is_none() {
        let (ptype, pid) = parse_parent(resp.get("parent"));
        if ptype == "page_id" {
            if let Some(page_id) = pid {
                let _ = storage.invalidate_page_detail(&ws_id, &page_id);
            }
        }
    }
    Ok(resp)
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
    let db = match client.get_database(&id).await {
        Ok(d) => d,
        Err(err) => {
            if NotionClient::is_authoritative_not_found(&err) {
                let _ = storage.mark_database_archived(&ws_id, &id);
            }
            return Err(err);
        }
    };
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
    let unfiltered = filter.is_none();
    let (rows, has_more) = client
        .query_database_all_with_meta(&id, filter, sorts, limit)
        .await?;
    let _ = storage.upsert_pages(&rows);
    if unfiltered && !has_more {
        let active_row_ids: HashSet<String> = rows.iter().map(|r| r.id.clone()).collect();
        let _ = storage.reconcile_database_rows_archived(&ws_id, &id, &active_row_ids);
    }
    Ok(rows)
}

fn property_map(extra: Option<Value>) -> Result<serde_json::Map<String, Value>> {
    match extra {
        Some(Value::Object(map)) => Ok(map),
        None => Ok(serde_json::Map::new()),
        Some(_) => bail!("properties must be a JSON object"),
    }
}

fn database_item_properties<'a>(
    columns: impl Iterator<Item = (&'a str, &'a str)>,
    title: Option<&str>,
    status: Option<&str>,
    extra: Option<Value>,
) -> Result<serde_json::Map<String, Value>> {
    let mut props = property_map(extra)?;
    let columns: Vec<_> = columns.collect();
    if let Some(title) = title {
        let (name, _) = columns.iter().copied().find(|(_, kind)| *kind == "title")
            .context("Database does not have a title property")?;
        props.insert(name.to_owned(), json!({ "title": [{ "type": "text", "text": { "content": title } }] }));
    }
    if let Some(status) = status.map(str::trim).filter(|s| !s.is_empty()) {
        let candidates: Vec<_> = columns.iter().copied()
            .filter(|(_, kind)| matches!(*kind, "status" | "select"))
            .collect();
        let explicit: Vec<_> = candidates.iter().copied().filter(|(name, _)| props.contains_key(*name)).collect();
        let status_columns: Vec<_> = candidates.iter().copied().filter(|(_, kind)| *kind == "status").collect();
        let choice = match (explicit.as_slice(), status_columns.as_slice(), candidates.as_slice()) {
            ([choice], _, _) | ([], [choice], _) | ([], [], [choice]) => *choice,
            _ => bail!("Cannot choose a status or select property. Specify the property in properties."),
        };
        let (name, kind) = choice;
        props.insert(name.to_owned(), json!({ (kind): { "name": status } }));
    }
    Ok(props)
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
    let db = op_get_database(client, storage, &db_id, false).await?;
    let columns = db.schema.iter().map(|(name, prop)| (name.as_str(), prop.prop_type.as_str()));
    let props = database_item_properties(columns, Some(title), status, extra_properties)?;

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
    let status = status.map(str::trim).filter(|s| !s.is_empty());
    let props = if title.is_some() || status.is_some() {
        let page = client.get_page(&id).await?;
        let columns = page.properties.iter().filter_map(|(name, value)| {
            let kind = match value {
                NotionPropertyValue::Title(_) => "title",
                NotionPropertyValue::Status(_) => "status",
                NotionPropertyValue::Select(_) => "select",
                _ => return None,
            };
            Some((name.as_str(), kind))
        });
        database_item_properties(columns, title, status, extra_properties)?
    } else {
        property_map(extra_properties)?
    };
    if props.is_empty() {
        bail!("Provide a title, status, or properties to update");
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    const KO_ITEM: &str = "- 목록 항목";
    const TWO_LINES: &str = "- first\n- second";

    #[test]
    fn parses_hyphen_text_for_all_audited_notion_commands() {
        // 1. pages create --title / --body
        let cli = parse_cli_from([
            "notion", "pages", "create", "--parent", "page", "--title", KO_ITEM, "--body",
            TWO_LINES, "--json",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Pages {
                action:
                    PagesCommand::Create {
                        parent,
                        title,
                        body,
                        json,
                        ..
                    },
            }) => {
                assert_eq!(parent, "page");
                assert_eq!(title, KO_ITEM);
                assert_eq!(body.as_deref(), Some(TWO_LINES));
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        // 2. pages append
        let cli = parse_cli_from(["notion", "pages", "append", "page", TWO_LINES, "--json"])
            .unwrap();
        match cli.command {
            Some(Command::Pages {
                action: PagesCommand::Append { page, body, json, .. },
            }) => {
                assert_eq!(page, "page");
                assert_eq!(body.as_deref(), Some(TWO_LINES));
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        // 3. blocks append
        let cli = parse_cli_from(["notion", "blocks", "append", "block", KO_ITEM, "--json"])
            .unwrap();
        match cli.command {
            Some(Command::Blocks {
                action: BlocksCommand::Append { block_id, body, json, .. },
            }) => {
                assert_eq!(block_id, "block");
                assert_eq!(body.as_deref(), Some(KO_ITEM));
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        // 4. blocks edit --text
        let cli = parse_cli_from(["notion", "blocks", "edit", "block", "--text", KO_ITEM, "--json"])
            .unwrap();
        match cli.command {
            Some(Command::Blocks {
                action: BlocksCommand::Edit { block_id, text, json },
            }) => {
                assert_eq!(block_id, "block");
                assert_eq!(text, KO_ITEM);
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        // 5. comments add
        let cli = parse_cli_from([
            "notion", "comments", "add", "page", KO_ITEM, "--discussion", "disc-1", "--json",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Comments {
                action: CommentsCommand::Add { page, text, discussion, json },
            }) => {
                assert_eq!(page, "page");
                assert_eq!(text, KO_ITEM);
                assert_eq!(discussion.as_deref(), Some("disc-1"));
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        // 6. db create-item --title / --body
        let cli = parse_cli_from([
            "notion", "db", "create-item", "db", "--title", KO_ITEM, "--body", TWO_LINES, "--json",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Db {
                action: DbCommand::CreateItem { database, title, body, json, .. },
            }) => {
                assert_eq!(database, "db");
                assert_eq!(title, KO_ITEM);
                assert_eq!(body.as_deref(), Some(TWO_LINES));
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        // 7. pages edit --title
        let cli = parse_cli_from(["notion", "pages", "edit", "page", "--title", KO_ITEM, "--json"])
            .unwrap();
        match cli.command {
            Some(Command::Pages {
                action: PagesCommand::Edit { page, title, json, .. },
            }) => {
                assert_eq!(page, "page");
                assert_eq!(title.as_deref(), Some(KO_ITEM));
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        // 8. db update-item --title
        let cli = parse_cli_from([
            "notion", "db", "update-item", "page", "--title", KO_ITEM, "--json",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Db {
                action: DbCommand::UpdateItem { page_id, title, json, .. },
            }) => {
                assert_eq!(page_id, "page");
                assert_eq!(title.as_deref(), Some(KO_ITEM));
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        // 9. search
        let cli = parse_cli_from(["notion", "search", KO_ITEM, "--remote", "--limit", "5", "--json"])
            .unwrap();
        match cli.command {
            Some(Command::Search {
                query,
                remote,
                limit,
                json,
                ..
            }) => {
                assert_eq!(query, KO_ITEM);
                assert!(remote);
                assert_eq!(limit, 5);
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn preserves_flags_missing_values_equals_separator_and_help() {
        // Explicit `--flag=value` and positional `--` still allow flag-looking text.
        let cli = parse_cli_from(["notion", "blocks", "edit", "blk", "--text=--dry-run", "--json"])
            .unwrap();
        match cli.command {
            Some(Command::Blocks {
                action: BlocksCommand::Edit { text, json, .. },
            }) => {
                assert_eq!(text, "--dry-run");
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let cli = parse_cli_from(["notion", "pages", "append", "page", "--", "--dry-run"])
            .unwrap();
        match cli.command {
            Some(Command::Pages {
                action: PagesCommand::Append { body, .. },
            }) => assert_eq!(body.as_deref(), Some("--dry-run")),
            other => panic!("unexpected command: {other:?}"),
        }

        let cli = parse_cli_from(["notion", "search", "--", "--help"]).unwrap();
        match cli.command {
            Some(Command::Search { query, .. }) => assert_eq!(query, "--help"),
            other => panic!("unexpected command: {other:?}"),
        }

        // Real flags after a missing option value must NOT be swallowed as the option value.
        for bad in [
            vec!["notion", "blocks", "edit", "blk", "--text", "--json"],
            vec!["notion", "blocks", "edit", "blk", "--text", "--dry-run"],
            vec!["notion", "blocks", "edit", "blk", "--text", "--help"],
            vec!["notion", "pages", "create", "--parent", "p", "--title", "T", "--body", "--json"],
            vec!["notion", "pages", "edit", "p", "--title", "--editor"],
            vec!["notion", "db", "create-item", "db", "--title", "--json"],
            vec!["notion", "pages", "append", "page", "--dry-run"],
            vec!["notion", "search", "--dry-run"],
            vec!["notion", "search", "-x"],
        ] {
            let err = parse_cli_from(bad.clone()).unwrap_err();
            assert_ne!(
                err.kind(),
                ErrorKind::DisplayHelp,
                "expected parse error for {bad:?}, got help"
            );
        }

        // `pages append page --json` keeps body=None (does not swallow `--json`).
        let cli = parse_cli_from(["notion", "pages", "append", "page", "--json"]).unwrap();
        match cli.command {
            Some(Command::Pages {
                action: PagesCommand::Append { body, json, .. },
            }) => {
                assert_eq!(body, None);
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        // Help works both alone and after hyphen text values.
        for help_args in [
            vec!["notion", "pages", "create", "--parent", "p", "--title", "T", "--body", KO_ITEM, "--help"],
            vec!["notion", "pages", "append", "page", KO_ITEM, "--help"],
            vec!["notion", "blocks", "append", "blk", KO_ITEM, "--help"],
            vec!["notion", "blocks", "edit", "blk", "--text", KO_ITEM, "--help"],
            vec!["notion", "comments", "add", "page", KO_ITEM, "--help"],
            vec!["notion", "db", "create-item", "db", "--title", "T", "--body", KO_ITEM, "--help"],
            vec!["notion", "pages", "edit", "page", "--title", KO_ITEM, "--help"],
            vec!["notion", "db", "update-item", "page", "--title", KO_ITEM, "--help"],
            vec!["notion", "search", KO_ITEM, "--help"],
            vec!["notion", "search", "--help"],
            vec!["notion", "blocks", "edit", "--help"],
            vec!["notion", "pages", "append", "page", "-h"],
        ] {
            let err = parse_cli_from(help_args.clone()).unwrap_err();
            assert_eq!(
                err.kind(),
                ErrorKind::DisplayHelp,
                "expected DisplayHelp for {help_args:?}, got {err}"
            );
        }

        // Non-text ID, number, and enum parsers stay unchanged.
        for rejected in [
            vec!["notion", "pages", "create", "--parent", "-bad-id", "--title", "T"],
            vec!["notion", "pages", "append", "-bad-page", KO_ITEM],
            vec!["notion", "pages", "list", "--limit", "-5"],
            vec!["notion", "pages", "get", "page", "--format", "-md"],
        ] {
            assert!(
                parse_cli_from(rejected.clone()).is_err(),
                "expected parser rejection for {rejected:?}"
            );
        }
    }
}
