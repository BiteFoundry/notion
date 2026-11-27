//! CLI argument definitions (`clap`) and hyphen-literal normalization helpers.

use std::{ffi::OsString, path::PathBuf};

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use serde_json::{json, Value};

use crate::{config::DEFAULT_SERVE_PORT, models::CodedError, notion::NotionApiError};

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
        /// Continue from the next_cursor of a previous --json reply
        #[arg(long)]
        cursor: Option<String>,
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
        /// Return every field: page properties, block tree, comments and media
        #[arg(long, requires = "json", conflicts_with = "compact")]
        full: bool,
        /// Compatibility alias: compact output is already the --json default
        #[arg(long, requires = "json")]
        compact: bool,
        /// Select specific standard fields or dynamic property names (comma-separated or repeatable)
        #[arg(long, requires = "json", conflicts_with = "full", value_delimiter = ',')]
        fields: Vec<String>,
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
        /// Return the full appended block objects instead of IDs
        #[arg(long, requires = "json")]
        full: bool,
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
        /// Maximum items to return [default: 50 with --json, all otherwise]
        #[arg(long)]
        limit: Option<usize>,
        /// Continue from the next_cursor of a previous --json reply
        #[arg(long)]
        cursor: Option<String>,
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
        /// Return the full appended block objects instead of IDs
        #[arg(long, requires = "json")]
        full: bool,
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
        /// Maximum items to return [default: 50 with --json, all otherwise]
        #[arg(long)]
        limit: Option<usize>,
        /// Continue from the next_cursor of a previous --json reply
        #[arg(long)]
        cursor: Option<String>,
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
        /// Continue from the next_cursor of a previous --json reply
        #[arg(long)]
        cursor: Option<String>,
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
        /// Maximum items to return [default: 50 with --json, all otherwise]
        #[arg(long)]
        limit: Option<usize>,
        /// Continue from the next_cursor of a previous --json reply
        #[arg(long)]
        cursor: Option<String>,
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
pub enum UsersCommand {
    /// List Notion workspace users
    List {
        /// Maximum items to return [default: 50 with --json, all otherwise]
        #[arg(long)]
        limit: Option<usize>,
        /// Continue from the next_cursor of a previous --json reply
        #[arg(long)]
        cursor: Option<String>,
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
    /// List Notion workspace users (`list`)
    Users {
        #[command(subcommand)]
        action: UsersCommand,
    },
    /// Download a Notion media URL or image/file/pdf block ID to local disk
    Download {
        target: String,
        #[arg(long)]
        out_dir: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Synchronize workspace pages & databases into local SQLite WAL + FTS5 cache. Streams every accessible object into the cache on the first run; later runs stop at the last synced edit time.
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
        /// API key required on `/api/v1/*` (or set `NOTION_SERVE_API_KEY`); generated per run when omitted
        #[arg(long)]
        api_key: Option<String>,
        /// Serve without an API key (loopback binds only)
        #[arg(long, alias = "no-auth")]
        allow_keyless: bool,
        /// Extra `Host` header name to accept besides loopback names (repeatable)
        #[arg(long, value_name = "HOST")]
        allowed_host: Vec<String>,
        /// Exact browser origin allowed to call the API with CORS, e.g. `https://app.example.com` (repeatable)
        #[arg(long, alias = "allowed-origin", value_name = "ORIGIN")]
        cors_origin: Vec<String>,
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
        [a, b] if a == "db" && b == "create-item" => Some((&["title", "body", "status"], None)),
        [a, b] if a == "db" && b == "update-item" => Some((&["title", "status"], None)),
        [a, b] if a == "comments" && b == "add" => Some((&[], Some(1))),
        [a] if a == "search" => Some((&[], Some(0))),
        _ => None,
    }
}

fn find_long_option<'a>(cmd: &'a clap::Command, name: &str) -> Option<&'a clap::Arg> {
    cmd.get_arguments().find(|arg| arg.get_long() == Some(name))
}

fn find_short_option(cmd: &clap::Command, ch: char) -> Option<&clap::Arg> {
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

fn is_text_slot_hyphen_literal(s: &str, cmd: &clap::Command) -> bool {
    if !could_be_hyphen_literal(s) {
        return false;
    }
    if s == "-" || s.starts_with("--") {
        return true;
    }
    let rest = &s[1..];
    if rest.chars().all(|c| {
        find_short_option(cmd, c).is_some_and(|arg| !arg.get_action().takes_values())
    }) {
        return false;
    }
    true
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

pub(super) fn normalize_cli_args(raw: Vec<OsString>) -> Vec<OsString> {
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
            } else if (s.len() == 2)
                .then(|| s.chars().nth(1))
                .flatten()
                .and_then(|ch| find_short_option(cmd, ch))
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
                                    ns.starts_with('-') && is_text_slot_hyphen_literal(ns, cmd)
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

            let is_literal_here = if affected_pos == Some(pos_idx) {
                is_text_slot_hyphen_literal(s, cmd)
            } else {
                is_separate_hyphen_literal(s, cmd)
            };
            if s.starts_with('-') && !is_literal_here {
                let takes_value = (s.len() == 2)
                    .then(|| s.chars().nth(1))
                    .flatten()
                    .and_then(|ch| find_short_option(cmd, ch))
                    .is_some_and(|arg| arg.get_action().takes_values());
                if takes_value {
                    if let Some(next) = tail
                        .get(i + 1)
                        .filter(|next| !is_flag_like_token(next, cmd) && next.to_str() != Some("--"))
                    {
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

pub(super) fn leaf_json_flag(mut matches: &clap::ArgMatches) -> bool {
    while let Some((_, sub)) = matches.subcommand() {
        matches = sub;
    }
    matches
        .try_get_one::<bool>("json")
        .ok()
        .flatten()
        .copied()
        .unwrap_or(false)
}

pub(super) fn error_envelope(err: &anyhow::Error) -> Value {
    let message = format!("{err:#}");
    if let Some(ce) = err.chain().find_map(|c| c.downcast_ref::<CodedError>()) {
        let mut obj = serde_json::Map::new();
        obj.insert("code".into(), json!(ce.code));
        obj.insert("message".into(), json!(message));
        if let Some(details) = &ce.details {
            obj.insert("details".into(), details.clone());
        }
        return json!({ "ok": false, "error": Value::Object(obj) });
    }
    let code = err
        .chain()
        .find_map(|c| c.downcast_ref::<NotionApiError>())
        .map(|e| e.code.as_str())
        .unwrap_or("error");
    json!({
        "ok": false,
        "error": {
            "code": code,
            "message": message,
        }
    })
}

