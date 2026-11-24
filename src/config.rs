//! Application configuration, multi-workspace profile registry (`config.toml`), local paths, and drafts.

use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const APP_DIR_NAME: &str = "notion-cli";
pub const KEYRING_SERVICE_NAME: &str = "notion-cli";
pub const DEFAULT_NOTION_API_BASE_URL: &str = "https://api.notion.com/v1";
pub const NOTION_API_VERSION: &str = "2022-06-28";
pub const DEFAULT_GITHUB_REPO: &str = "BiteFoundry/notion";
pub const DEFAULT_SERVE_PORT: u16 = 8791;
pub const DEFAULT_ACTIVE_POLL_MS: u64 = 5_000;
pub const LEGACY_DEFAULT_ACTIVE_POLL_MS: u64 = 15_000;
pub const DEFAULT_SYNC_POLL_MS: u64 = 60_000;

/// TUI page canvas horizontal width mode (`Centered` editorial measure vs `Full` width).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PageWidthMode {
    #[default]
    Centered,
    Full,
}

impl PageWidthMode {
    pub fn toggle(self) -> Self {
        match self {
            Self::Centered => Self::Full,
            Self::Full => Self::Centered,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Centered => "Centered",
            Self::Full => "Full",
        }
    }
}

/// List density mode in TUI (`Cozy` default vs `Compact`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DensityMode {
    #[default]
    Cozy,
    Compact,
}

impl DensityMode {
    pub fn toggle(self) -> Self {
        match self {
            Self::Cozy => Self::Compact,
            Self::Compact => Self::Cozy,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Cozy => "Cozy",
            Self::Compact => "Compact",
        }
    }
}

/// Terminal color theme preset (`Auto` detects `NO_COLOR`, `COLORTERM`, and `TERM`).
/// Includes 5 curated Notion-inspired truecolor palettes plus ANSI/NoColor modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemePreset {
    #[default]
    Auto,
    #[serde(alias = "dark", alias = "clean-dark")]
    CleanDark,
    #[serde(alias = "light", alias = "warm-paper")]
    WarmPaper,
    #[serde(alias = "tokyo-night")]
    TokyoNight,
    Catppuccin,
    Nord,
    Ansi256,
    Ansi16,
    NoColor,
}

impl ThemePreset {
    pub fn next(self) -> Self {
        match self {
            Self::Auto => Self::CleanDark,
            Self::CleanDark => Self::WarmPaper,
            Self::WarmPaper => Self::TokyoNight,
            Self::TokyoNight => Self::Catppuccin,
            Self::Catppuccin => Self::Nord,
            Self::Nord => Self::Ansi256,
            Self::Ansi256 => Self::Ansi16,
            Self::Ansi16 => Self::NoColor,
            Self::NoColor => Self::Auto,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::CleanDark => "Clean Dark",
            Self::WarmPaper => "Warm Paper",
            Self::TokyoNight => "Tokyo Night",
            Self::Catppuccin => "Catppuccin",
            Self::Nord => "Nord",
            Self::Ansi256 => "ANSI 256",
            Self::Ansi16 => "ANSI 16",
            Self::NoColor => "No Color",
        }
    }
}

/// Terminal image rendering protocol preference (`auto` detects Kitty / iTerm2 / Sixel / Halfblock).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageProtocolPreference {
    #[default]
    Auto,
    Kitty,
    #[serde(alias = "iterm")]
    Iterm2,
    Sixel,
    #[serde(alias = "halfblocks", alias = "ansi")]
    Halfblock,
}

impl ImageProtocolPreference {
    pub fn parse_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Some(Self::Auto),
            "kitty" => Some(Self::Kitty),
            "iterm2" | "iterm" => Some(Self::Iterm2),
            "sixel" => Some(Self::Sixel),
            "halfblock" | "halfblocks" | "ansi" => Some(Self::Halfblock),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Kitty => "kitty",
            Self::Iterm2 => "iterm2",
            Self::Sixel => "sixel",
            Self::Halfblock => "halfblock",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Kitty => "Kitty",
            Self::Iterm2 => "iTerm2",
            Self::Sixel => "Sixel",
            Self::Halfblock => "Halfblock",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Auto => Self::Kitty,
            Self::Kitty => Self::Iterm2,
            Self::Iterm2 => Self::Sixel,
            Self::Sixel => Self::Halfblock,
            Self::Halfblock => Self::Auto,
        }
    }
}

/// Named workspace profile stored in `config.toml` (secrets live in OS keyring / `credentials.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct WorkspaceProfile {
    /// Local profile slug (e.g. `"default"`, `"work"`, `"personal"`).
    pub name: String,
    /// Notion workspace ID (from OAuth or bot metadata, falls back to profile name).
    pub workspace_id: String,
    /// Human-readable Notion workspace name.
    pub workspace_name: String,
    /// Optional workspace emoji/icon.
    pub workspace_icon: Option<String>,
    /// Bot user ID returned by `/v1/users/me` or OAuth token exchange.
    pub bot_id: Option<String>,
    /// Bot display name.
    pub bot_name: Option<String>,
    /// `"integration"` or `"oauth"`.
    pub auth_mode: String,
    /// Optional per-workspace Notion API base URL override.
    pub api_base_url: Option<String>,
}

impl WorkspaceProfile {
    pub fn display_name(&self) -> &str {
        if !self.workspace_name.trim().is_empty() {
            &self.workspace_name
        } else {
            &self.name
        }
    }

    pub fn effective_workspace_id(&self) -> &str {
        if !self.workspace_id.trim().is_empty() {
            &self.workspace_id
        } else if !self.name.trim().is_empty() {
            &self.name
        } else {
            "default"
        }
    }
}

/// A field value that came from an environment variable or a command-line flag, kept next to
/// the value the config file holds so that saving does not write the override into the file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Overridden<T> {
    applied: T,
    persisted: T,
}

impl<T: Clone + PartialEq> Overridden<T> {
    /// Replaces `field` with `applied` and remembers what the file held.
    fn apply(slot: &mut Option<Self>, field: &mut T, applied: T) {
        // A second override of the same field keeps the first persisted value.
        let persisted = match slot.take() {
            Some(earlier) if earlier.applied == *field => earlier.persisted,
            _ => field.clone(),
        };
        *field = applied.clone();
        *slot = Some(Self { applied, persisted });
    }

    /// Puts the file's value back unless the field was changed after the override.
    fn restore(slot: &Option<Self>, field: &mut T) {
        if let Some(overridden) = slot {
            if *field == overridden.applied {
                *field = overridden.persisted.clone();
            }
        }
    }
}

/// Per-run overrides of [`AppConfig`] fields. Never written to the config file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransientOverrides {
    active_workspace: Option<Overridden<String>>,
    api_base_url: Option<Overridden<String>>,
    oauth_client_id: Option<Overridden<String>>,
    github_repo: Option<Overridden<String>>,
    cli_cache_ttl_secs: Option<Overridden<u64>>,
    auto_daemon: Option<Overridden<bool>>,
    image_protocol: Option<Overridden<ImageProtocolPreference>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default = "AppConfig::file_defaults")]
pub struct AppConfig {
    /// Active workspace profile slug.
    pub active_workspace: String,
    /// Multi-workspace profile registry.
    pub workspaces: Vec<WorkspaceProfile>,
    /// Base URL for Notion REST API v1 (overridable via `NOTION_API_BASE_URL` for mock E2E tests).
    pub api_base_url: String,
    /// Optional Public OAuth 2.0 client ID.
    pub oauth_client_id: String,
    /// GitHub `owner/repo` used by `notion update`.
    pub github_repo: String,
    /// Enable desktop toast/bell notifications in TUI.
    pub notify: bool,
    /// Show left workspace sidebar in wide/medium layout.
    pub show_sidebar: bool,
    /// Editorial centered measure (`centered`) vs full width (`full`).
    pub page_width: PageWidthMode,
    /// Render inline image thumbnails directly inside Notion page canvas.
    pub inline_images: bool,
    /// Show inline hyperlink `[href]` URLs next to link text in Notion page canvas (toggleable with `h`).
    pub show_inline_hrefs: bool,
    /// Preferred terminal image rendering protocol (`auto`, `kitty`, `iterm2`, `sixel`, `halfblock`).
    pub image_protocol: ImageProtocolPreference,
    /// Custom download directory. Empty defaults to `Downloads/notion`.
    pub download_dir: String,
    /// Active page/database refresh poll interval (ms) in TUI.
    pub active_poll_ms: u64,
    /// Background workspace tree sync interval (ms).
    pub sync_poll_ms: u64,
    /// CLI local cache TTL in seconds (`NOTION_CLI_CACHE_SECS`, default `0`).
    pub cli_cache_ttl_secs: u64,
    /// Automatically start and attach to the lightweight background sync daemon (`NOTION_AUTO_DAEMON=1|true`).
    pub auto_daemon: bool,
    /// TUI list density (`cozy` or `compact`).
    pub density: DensityMode,
    /// TUI color theme preset.
    pub theme: ThemePreset,
    /// Values overridden for this run by environment variables or `--workspace`.
    #[doc(hidden)]
    #[serde(skip)]
    pub transient: TransientOverrides,
}

impl Default for AppConfig {
    /// The defaults with the process environment's overrides applied, as [`AppConfig::load`]
    /// gives when there is no config file.
    fn default() -> Self {
        let mut cfg = Self::file_defaults();
        cfg.apply_env(&process_env);
        cfg
    }
}

impl AppConfig {
    /// The value of every field a config file does not set. Reads no environment variable.
    fn file_defaults() -> Self {
        Self {
            active_workspace: "default".to_string(),
            workspaces: Vec::new(),
            api_base_url: DEFAULT_NOTION_API_BASE_URL.to_string(),
            oauth_client_id: String::new(),
            github_repo: DEFAULT_GITHUB_REPO.to_string(),
            notify: true,
            show_sidebar: true,
            page_width: PageWidthMode::Centered,
            inline_images: true,
            show_inline_hrefs: false,
            image_protocol: ImageProtocolPreference::default(),
            download_dir: String::new(),
            active_poll_ms: DEFAULT_ACTIVE_POLL_MS,
            sync_poll_ms: DEFAULT_SYNC_POLL_MS,
            cli_cache_ttl_secs: 0,
            auto_daemon: false,
            density: DensityMode::default(),
            theme: ThemePreset::default(),
            transient: TransientOverrides::default(),
        }
    }

    /// Loads the config file and applies the environment's overrides for this run.
    pub fn load() -> Self {
        Self::load_with(&config_file_path(), process_env)
    }

    /// [`AppConfig::load`] with the file and the environment reader given. A file that is
    /// missing or does not parse counts as empty; the overrides apply either way.
    pub fn load_with(path: &Path, env: impl Fn(&str) -> Option<String>) -> Self {
        let mut cfg = Self::load_from(path).unwrap_or_else(|_| Self::file_defaults());
        cfg.apply_env(&env);
        cfg
    }

    fn apply_env(&mut self, env: &dyn Fn(&str) -> Option<String>) {
        let non_empty = |key: &str| {
            env(key)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };
        let transient = &mut self.transient;
        if let Some(ws) = non_empty("NOTION_WORKSPACE") {
            Overridden::apply(&mut transient.active_workspace, &mut self.active_workspace, ws);
        }
        if let Some(url) = non_empty("NOTION_API_BASE_URL") {
            Overridden::apply(&mut transient.api_base_url, &mut self.api_base_url, url);
        }
        if let Some(cid) = non_empty("NOTION_OAUTH_CLIENT_ID") {
            Overridden::apply(&mut transient.oauth_client_id, &mut self.oauth_client_id, cid);
        }
        if let Some(repo) = non_empty("NOTION_GITHUB_REPO") {
            Overridden::apply(&mut transient.github_repo, &mut self.github_repo, repo);
        }
        if let Some(ttl) = env("NOTION_CLI_CACHE_SECS").and_then(|s| s.trim().parse::<u64>().ok()) {
            Overridden::apply(&mut transient.cli_cache_ttl_secs, &mut self.cli_cache_ttl_secs, ttl);
        }
        if let Some(auto) = env("NOTION_AUTO_DAEMON").and_then(|s| parse_env_bool(&s)) {
            Overridden::apply(&mut transient.auto_daemon, &mut self.auto_daemon, auto);
        }
        if let Some(proto) =
            non_empty("NOTION_IMAGE_PROTOCOL").and_then(|s| ImageProtocolPreference::parse_str(&s))
        {
            Overridden::apply(&mut transient.image_protocol, &mut self.image_protocol, proto);
        }
    }

    /// Uses `name` as the active workspace for this run only (`--workspace`). Saving the config
    /// keeps the file's own active workspace.
    pub fn override_workspace(&mut self, name: impl Into<String>) {
        Overridden::apply(
            &mut self.transient.active_workspace,
            &mut self.active_workspace,
            name.into(),
        );
    }

    /// Makes `name` the active workspace for good: the next save writes it, even when it is
    /// the same name a run-only override gave.
    pub fn set_active_workspace(&mut self, name: impl Into<String>) {
        self.active_workspace = name.into();
        self.transient.active_workspace = None;
    }

    /// The config as the file should hold it: overridden fields are back at the file's value
    /// unless they were changed after the override.
    fn persisted(&self) -> Self {
        let mut cfg = self.clone();
        let t = &self.transient;
        Overridden::restore(&t.active_workspace, &mut cfg.active_workspace);
        Overridden::restore(&t.api_base_url, &mut cfg.api_base_url);
        Overridden::restore(&t.oauth_client_id, &mut cfg.oauth_client_id);
        Overridden::restore(&t.github_repo, &mut cfg.github_repo);
        Overridden::restore(&t.cli_cache_ttl_secs, &mut cfg.cli_cache_ttl_secs);
        Overridden::restore(&t.auto_daemon, &mut cfg.auto_daemon);
        Overridden::restore(&t.image_protocol, &mut cfg.image_protocol);
        cfg
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;
        toml::from_str(&raw)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&config_file_path())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        ensure_parent(path)?;
        let raw = toml::to_string_pretty(&self.persisted()).context("Failed to serialize config")?;
        write_atomic(path, raw.as_bytes())
    }

    pub fn resolved_download_dir(&self) -> PathBuf {
        if self.download_dir.trim().is_empty() {
            default_download_dir()
        } else {
            expand_tilde(self.download_dir.trim())
        }
    }

    /// Resolves the effective active page/database poll interval in milliseconds.
    /// Profiles saved with the legacy v0.1.1 default (`15_000` ms) automatically receive
    /// the new `5_000` ms default; any other explicit user value is preserved.
    pub fn effective_active_poll_ms(&self) -> u64 {
        if self.active_poll_ms == LEGACY_DEFAULT_ACTIVE_POLL_MS {
            DEFAULT_ACTIVE_POLL_MS
        } else {
            self.active_poll_ms
        }
    }

    /// Resolves the active workspace profile name (`override_name` -> `active_workspace` -> `"default"`).
    pub fn effective_workspace_name(&self, override_name: Option<&str>) -> String {
        if let Some(name) = override_name.map(str::trim).filter(|s| !s.is_empty()) {
            return name.to_string();
        }
        let active = self.active_workspace.trim();
        if !active.is_empty() {
            active.to_string()
        } else if let Some(first) = self.workspaces.first() {
            first.name.clone()
        } else {
            "default".to_string()
        }
    }

    pub fn find_workspace(&self, name_or_id: &str) -> Option<&WorkspaceProfile> {
        let needle = name_or_id.trim();
        if needle.is_empty() {
            return None;
        }
        self.workspaces.iter().find(|w| {
            w.name.eq_ignore_ascii_case(needle)
                || w.workspace_id.eq_ignore_ascii_case(needle)
                || w.workspace_name.eq_ignore_ascii_case(needle)
        })
    }

    pub fn active_profile(&self) -> Option<&WorkspaceProfile> {
        let name = self.effective_workspace_name(None);
        self.find_workspace(&name).or_else(|| self.workspaces.first())
    }

    pub fn upsert_workspace(&mut self, profile: WorkspaceProfile, make_active: bool) {
        if let Some(existing) = self
            .workspaces
            .iter_mut()
            .find(|w| w.name.eq_ignore_ascii_case(&profile.name))
        {
            *existing = profile.clone();
        } else {
            self.workspaces.push(profile.clone());
        }
        if make_active || self.active_workspace.trim().is_empty() {
            self.set_active_workspace(profile.name);
        }
    }

    pub fn remove_workspace(&mut self, name: &str) -> bool {
        let before = self.workspaces.len();
        self.workspaces
            .retain(|w| !w.name.eq_ignore_ascii_case(name.trim()));
        if self.active_workspace.eq_ignore_ascii_case(name.trim()) {
            let next = self
                .workspaces
                .first()
                .map(|w| w.name.clone())
                .unwrap_or_else(|| "default".to_string());
            self.set_active_workspace(next);
        }
        self.workspaces.len() != before
    }

    pub fn effective_api_base_url(&self) -> String {
        if let Some(env_url) = first_non_empty_env(&["NOTION_API_BASE_URL"]) {
            return env_url;
        }
        if let Some(prof) = self.active_profile() {
            if let Some(ref url) = prof.api_base_url {
                if !url.trim().is_empty() {
                    return url.trim().to_string();
                }
            }
        }
        if self.api_base_url.trim().is_empty() {
            DEFAULT_NOTION_API_BASE_URL.to_string()
        } else {
            self.api_base_url.trim().to_string()
        }
    }
}

pub fn config_dir() -> PathBuf {
    if let Ok(override_dir) = std::env::var("NOTION_CONFIG_DIR") {
        let trimmed = override_dir.trim();
        if !trimmed.is_empty() {
            return expand_tilde(trimmed);
        }
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_DIR_NAME)
}

pub fn data_dir() -> PathBuf {
    if let Ok(override_dir) = std::env::var("NOTION_DATA_DIR") {
        let trimmed = override_dir.trim();
        if !trimmed.is_empty() {
            return expand_tilde(trimmed);
        }
    }
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_DIR_NAME)
}

/// Canonicalizes the deepest existing ancestor of `path` and re-appends any non-existent
/// trailing components, falling back to `std::path::absolute`.
pub fn canonicalize_existing_ancestor(path: &Path) -> PathBuf {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut cur: &Path = abs.as_path();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if cur.exists() {
            if let Ok(mut canon) = fs::canonicalize(cur) {
                for comp in tail.into_iter().rev() {
                    canon.push(comp);
                }
                return canon;
            }
            break;
        }
        match (cur.parent(), cur.file_name()) {
            (Some(parent), Some(name)) if parent != cur => {
                tail.push(name.to_os_string());
                cur = parent;
            }
            _ => break,
        }
    }
    abs
}

pub fn config_file_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn credential_fallback_path() -> PathBuf {
    config_dir().join("credentials.json")
}

pub fn drafts_path() -> PathBuf {
    config_dir().join("drafts.json")
}

pub fn media_cache_dir() -> PathBuf {
    data_dir().join("media_cache")
}

pub fn db_path(workspace_id: Option<&str>) -> PathBuf {
    let bucket = workspace_id
        .filter(|id| !id.trim().is_empty())
        .unwrap_or("default");
    let sanitized: String = bucket
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    data_dir().join(sanitized).join("cache.db")
}

pub fn default_download_dir() -> PathBuf {
    dirs::download_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("notion")
}

pub fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
    }
    Ok(())
}

static ATOMIC_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Atomically writes `data` to `path` using a unique temp file (`create_new(true)`),
/// flushing before rename. Files containing `"credentials"` or `"drafts"` default to `0o600` on Unix.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let is_secret = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| {
            let lower = n.to_ascii_lowercase();
            lower.contains("credentials") || lower.contains("drafts")
        })
        .unwrap_or(false);
    write_atomic_with_mode(path, data, is_secret)
}

/// Atomically writes `data` to `path` with `0o600` permissions at creation time on Unix.
pub fn write_atomic_0600(path: &Path, data: &[u8]) -> Result<()> {
    write_atomic_with_mode(path, data, true)
}

fn write_atomic_with_mode(path: &Path, data: &[u8], mode_0600: bool) -> Result<()> {
    let target_path: PathBuf = match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            if mode_0600 {
                anyhow::bail!(
                    "Refusing to write sensitive file through symlink: {}",
                    path.display()
                );
            }
            let resolved = fs::canonicalize(path).with_context(|| {
                format!(
                    "Refusing to write through dangling or unreadable symlink: {}",
                    path.display()
                )
            })?;
            let target_meta = fs::metadata(&resolved).with_context(|| {
                format!("Failed to inspect symlink target: {}", resolved.display())
            })?;
            if target_meta.is_dir() {
                anyhow::bail!(
                    "Refusing to overwrite directory via symlink: {} -> {}",
                    path.display(),
                    resolved.display()
                );
            }
            resolved
        }
        Ok(meta) if meta.is_dir() => {
            anyhow::bail!("Refusing to overwrite directory: {}", path.display());
        }
        _ => path.to_path_buf(),
    };

    ensure_parent(&target_path)?;
    let parent = target_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = target_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let pid = std::process::id();
    let seq = ATOMIC_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".{name}.{pid}.{seq}.tmp"));

    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    if mode_0600 {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = mode_0600;

    let write_res = (|| -> Result<()> {
        let mut file = opts
            .open(&tmp)
            .with_context(|| format!("Failed to create temp file: {}", tmp.display()))?;
        file.write_all(data)
            .with_context(|| format!("Failed to write temp file: {}", tmp.display()))?;
        file.flush()
            .with_context(|| format!("Failed to flush temp file: {}", tmp.display()))?;
        Ok(())
    })();

    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }

    if let Err(rename_err) = fs::rename(&tmp, &target_path) {
        if let Err(replace_err) =
            replace_existing_with_backup(&tmp, &target_path, parent, name, pid, seq, rename_err)
        {
            let _ = fs::remove_file(&tmp);
            return Err(replace_err);
        }
    }
    Ok(())
}

fn replace_existing_with_backup(
    tmp: &Path,
    path: &Path,
    parent: &Path,
    name: &str,
    pid: u32,
    seq: u64,
    original_err: std::io::Error,
) -> Result<()> {
    if path.symlink_metadata().is_err() {
        return Err(original_err)
            .with_context(|| format!("Failed to save file: {}", path.display()));
    }
    let backup = parent.join(format!(".{name}.{pid}.{seq}.bak"));
    fs::rename(path, &backup)
        .with_context(|| format!("Failed to back up existing file: {}", path.display()))?;
    match fs::rename(tmp, path) {
        Ok(()) => {
            let _ = fs::remove_file(&backup);
            Ok(())
        }
        Err(err) => {
            let _ = fs::rename(&backup, path);
            Err(err).with_context(|| format!("Atomic file save failed: {}", path.display()))
        }
    }
}

/// Persistent draft store backed by `drafts.json` (`0600` permissions on Unix).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DraftStore {
    pub text: HashMap<String, String>,
}

impl DraftStore {
    pub fn load() -> Self {
        Self::load_from(&drafts_path()).unwrap_or_default()
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(path)?;
        if raw.trim().is_empty() {
            return Ok(Self::default());
        }
        Ok(serde_json::from_str::<Self>(&raw).unwrap_or_default())
    }

    pub fn save(&self) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(self)?;
        write_atomic_0600(&drafts_path(), &bytes)
    }
}

fn process_env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

fn first_non_empty_env(keys: &[&str]) -> Option<String> {
    for &key in keys {
        if let Ok(val) = std::env::var(key) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn parse_env_bool(input: &str) -> Option<bool> {
    match input.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

pub(crate) fn expand_tilde(input: &str) -> PathBuf {
    if input == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"));
    }
    if let Some(rest) = input
        .strip_prefix("~/")
        .or_else(|| input.strip_prefix("~\\"))
    {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(input)
}

#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
pub(crate) struct EnvVarGuard {
    saved: Vec<(&'static str, Option<String>)>,
}

#[cfg(test)]
impl EnvVarGuard {
    pub(crate) fn set(vars: &[(&'static str, Option<&str>)]) -> Self {
        let mut saved = Vec::with_capacity(vars.len());
        for &(key, val) in vars {
            saved.push((key, std::env::var(key).ok()));
            match val {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        Self { saved }
    }
}

#[cfg(test)]
impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        for (key, orig) in self.saved.drain(..) {
            match orig {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_app_config_workspace_registry_and_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let mut cfg = AppConfig::default();
        cfg.page_width = PageWidthMode::Full;
        cfg.theme = ThemePreset::WarmPaper;
        cfg.upsert_workspace(
            WorkspaceProfile {
                name: "acme".into(),
                workspace_id: "ws-acme-1".into(),
                workspace_name: "Acme HQ".into(),
                workspace_icon: Some("🚀".into()),
                bot_id: Some("bot-1".into()),
                bot_name: Some("Acme Bot".into()),
                auth_mode: "integration".into(),
                api_base_url: None,
            },
            true,
        );
        cfg.upsert_workspace(
            WorkspaceProfile {
                name: "personal".into(),
                workspace_id: "ws-me-2".into(),
                workspace_name: "Personal Wiki".into(),
                workspace_icon: None,
                bot_id: None,
                bot_name: None,
                auth_mode: "oauth".into(),
                api_base_url: None,
            },
            false,
        );
        cfg.save_to(&path).unwrap();

        let loaded = AppConfig::load_from(&path).unwrap();
        assert_eq!(loaded.active_workspace, "acme");
        assert_eq!(loaded.workspaces.len(), 2);
        assert_eq!(loaded.page_width, PageWidthMode::Full);
        assert_eq!(loaded.theme, ThemePreset::WarmPaper);
        assert_eq!(loaded.find_workspace("Acme HQ").unwrap().name, "acme");
    }

    fn env_of<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| vars.iter().find(|(k, _)| *k == key).map(|(_, v)| v.to_string())
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn env_overrides_apply_without_a_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let cfg = AppConfig::load_with(
            &path,
            env_of(&[
                ("NOTION_API_BASE_URL", " http://127.0.0.1:9000/v1 "),
                ("NOTION_WORKSPACE", "ci"),
                ("NOTION_CLI_CACHE_SECS", "30"),
                ("NOTION_AUTO_DAEMON", "true"),
                ("NOTION_IMAGE_PROTOCOL", "halfblock"),
            ]),
        );
        assert_eq!(cfg.api_base_url, "http://127.0.0.1:9000/v1");
        assert_eq!(cfg.active_workspace, "ci");
        assert_eq!(cfg.cli_cache_ttl_secs, 30);
        assert!(cfg.auto_daemon);
        assert_eq!(cfg.image_protocol, ImageProtocolPreference::parse_str("halfblock").unwrap());

        // Saving the first config of a fresh install must not turn the overrides into settings.
        cfg.save_to(&path).unwrap();
        let reloaded = AppConfig::load_with(&path, no_env);
        assert_eq!(reloaded.api_base_url, DEFAULT_NOTION_API_BASE_URL);
        assert_eq!(reloaded.active_workspace, "default");
        assert_eq!(reloaded.cli_cache_ttl_secs, 0);
        assert!(!reloaded.auto_daemon);
        assert_eq!(reloaded.image_protocol, ImageProtocolPreference::default());

        // Unusable values are ignored, as before.
        let ignored = AppConfig::load_with(
            &dir.path().join("none.toml"),
            env_of(&[("NOTION_WORKSPACE", "  "), ("NOTION_CLI_CACHE_SECS", "soon")]),
        );
        assert_eq!(ignored.active_workspace, "default");
        assert_eq!(ignored.cli_cache_ttl_secs, 0);
    }

    #[test]
    fn overrides_are_not_saved_but_deliberate_changes_are() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "active_workspace = \"home\"\noauth_client_id = \"file-client\"\ngithub_repo = \"me/fork\"\n",
        )
        .unwrap();
        let env = [
            ("NOTION_WORKSPACE", "ci"),
            ("NOTION_OAUTH_CLIENT_ID", "env-client"),
            ("NOTION_GITHUB_REPO", "ci/repo"),
        ];

        // Saved while overridden (a TUI preference toggle, say): the file keeps its own values.
        let mut cfg = AppConfig::load_with(&path, env_of(&env));
        assert_eq!(cfg.active_workspace, "ci");
        cfg.notify = false;
        cfg.save_to(&path).unwrap();
        let reloaded = AppConfig::load_with(&path, no_env);
        assert_eq!(reloaded.active_workspace, "home");
        assert_eq!(reloaded.oauth_client_id, "file-client");
        assert_eq!(reloaded.github_repo, "me/fork");
        assert!(!reloaded.notify, "the real change is saved");

        // A field changed after the override is saved.
        let mut cfg = AppConfig::load_with(&path, env_of(&env));
        cfg.active_workspace = "other".into();
        cfg.save_to(&path).unwrap();
        assert_eq!(AppConfig::load_with(&path, no_env).active_workspace, "other");

        // --workspace is an override too, on top of the environment's.
        let mut cfg = AppConfig::load_with(&path, env_of(&env));
        cfg.override_workspace("flag");
        assert_eq!(cfg.active_workspace, "flag");
        cfg.save_to(&path).unwrap();
        assert_eq!(AppConfig::load_with(&path, no_env).active_workspace, "other");

        // Choosing the overridden name on purpose makes it the saved one.
        let mut cfg = AppConfig::load_with(&path, env_of(&env));
        cfg.set_active_workspace("ci");
        cfg.save_to(&path).unwrap();
        assert_eq!(AppConfig::load_with(&path, no_env).active_workspace, "ci");

        let mut cfg = AppConfig::load_with(&path, no_env);
        cfg.override_workspace("fresh");
        cfg.upsert_workspace(
            WorkspaceProfile { name: "fresh".into(), ..WorkspaceProfile::default() },
            true,
        );
        cfg.save_to(&path).unwrap();
        assert_eq!(AppConfig::load_with(&path, no_env).active_workspace, "fresh");
    }

    #[test]
    fn save_without_overrides_writes_every_field_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut cfg = AppConfig::load_with(&path, no_env);
        cfg.active_workspace = "home".into();
        cfg.cli_cache_ttl_secs = 12;
        cfg.save_to(&path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), toml::to_string_pretty(&cfg).unwrap());
        assert_eq!(AppConfig::load_with(&path, no_env), cfg);
    }

    #[test]
    fn test_theme_preset_and_width_cycles() {
        let mut p = ThemePreset::Auto;
        for _ in 0..9 {
            p = p.next();
        }
        assert_eq!(p, ThemePreset::Auto);
        assert_eq!(PageWidthMode::Centered.toggle(), PageWidthMode::Full);
        assert_eq!(PageWidthMode::Full.toggle(), PageWidthMode::Centered);
    }

    #[test]
    fn test_active_poll_ms_default_legacy_15000_and_explicit_preferences() {
        let def = AppConfig::default();
        assert_eq!(def.active_poll_ms, DEFAULT_ACTIVE_POLL_MS);
        assert_eq!(def.effective_active_poll_ms(), 5_000);
        assert_eq!(def.sync_poll_ms, 60_000);

        let dir = tempfile::tempdir().unwrap();
        let legacy_path = dir.path().join("legacy.toml");
        fs::write(&legacy_path, "active_poll_ms = 15000\nsync_poll_ms = 60000\n").unwrap();
        let legacy = AppConfig::load_from(&legacy_path).unwrap();
        assert_eq!(legacy.effective_active_poll_ms(), 5_000);
        assert_eq!(legacy.sync_poll_ms, 60_000);

        let custom_slow_path = dir.path().join("custom_slow.toml");
        fs::write(&custom_slow_path, "active_poll_ms = 20000\nsync_poll_ms = 90000\n").unwrap();
        let custom_slow = AppConfig::load_from(&custom_slow_path).unwrap();
        assert_eq!(custom_slow.effective_active_poll_ms(), 20_000);
        assert_eq!(custom_slow.sync_poll_ms, 90_000);

        let custom_fast_path = dir.path().join("custom_fast.toml");
        fs::write(&custom_fast_path, "active_poll_ms = 3000\n").unwrap();
        let custom_fast = AppConfig::load_from(&custom_fast_path).unwrap();
        assert_eq!(custom_fast.effective_active_poll_ms(), 3_000);

        let omitted_path = dir.path().join("omitted.toml");
        fs::write(&omitted_path, "notify = true\n").unwrap();
        let omitted = AppConfig::load_from(&omitted_path).unwrap();
        assert_eq!(omitted.effective_active_poll_ms(), 5_000);
    }

    #[test]
    fn canonicalize_existing_ancestor_resolves_existing_prefix_and_keeps_missing_tail() {
        let dir = tempfile::tempdir().unwrap();
        let canonical_root = fs::canonicalize(dir.path()).unwrap();

        let existing_file = dir.path().join("exists.txt");
        fs::write(&existing_file, b"ok").unwrap();
        assert_eq!(
            canonicalize_existing_ancestor(&existing_file),
            canonical_root.join("exists.txt")
        );

        let nested_missing = dir.path().join("sub").join("deep").join("profile");
        assert_eq!(
            canonicalize_existing_ancestor(&nested_missing),
            canonical_root.join("sub").join("deep").join("profile")
        );

        let dotted = dir.path().join(".").join("sub").join("..").join("leaf");
        assert_eq!(
            canonicalize_existing_ancestor(&dotted),
            canonical_root.join("leaf")
        );
    }

    #[tokio::test]
    async fn config_and_data_dir_expand_tilde() {
        let _lock = TEST_ENV_LOCK.lock().await;
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let _guard = EnvVarGuard::set(&[
            ("NOTION_CONFIG_DIR", Some("~/notion-test-cfg")),
            ("NOTION_DATA_DIR", Some("~/notion-test-data")),
        ]);
        assert_eq!(config_dir(), home.join("notion-test-cfg"));
        assert_eq!(data_dir(), home.join("notion-test-data"));
    }

    #[cfg(any(unix, windows))]
    fn create_test_file_symlink(original: &Path, link: &Path) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(original, link)
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_file(original, link)
        }
    }

    #[cfg(any(unix, windows))]
    fn create_test_dir_symlink(original: &Path, link: &Path) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(original, link)
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_dir(original, link)
        }
    }

    #[test]
    fn write_atomic_with_mode_hardens_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let target_dir = tempfile::tempdir().unwrap();
        let real_file = target_dir.path().join("dotfiles-config.toml");
        fs::write(&real_file, b"old-config").unwrap();

        let link_cfg = dir.path().join("config.toml");
        match create_test_file_symlink(&real_file, &link_cfg) {
            Ok(()) => {}
            #[cfg(windows)]
            Err(err) if err.raw_os_error() == Some(1314) => {
                // Unprivileged Windows without Developer Mode cannot create symlinks.
                return;
            }
            Err(err) => panic!("failed to create test symlink: {err}"),
        }

        // mode_0600 == false resolves target, updates it atomically, and preserves the symlink.
        write_atomic_with_mode(&link_cfg, b"new-config", false).unwrap();
        assert!(fs::symlink_metadata(&link_cfg)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(&real_file).unwrap(), b"new-config");
        assert_eq!(fs::read(&link_cfg).unwrap(), b"new-config");

        // mode_0600 == true refuses writing through any symlink (e.g. credentials.json).
        let secret_target = target_dir.path().join("real-creds.json");
        fs::write(&secret_target, b"original-secret").unwrap();
        let secret_link = dir.path().join("credentials.json");
        create_test_file_symlink(&secret_target, &secret_link).unwrap();
        let err = write_atomic_0600(&secret_link, b"overwritten").unwrap_err();
        assert!(
            err.to_string().contains("symlink"),
            "unexpected error: {err}"
        );
        assert_eq!(fs::read(&secret_target).unwrap(), b"original-secret");

        // Dangling symlink errors even when mode_0600 == false.
        let dangling_link = dir.path().join("dangling.toml");
        create_test_file_symlink(&target_dir.path().join("does-not-exist.toml"), &dangling_link)
            .unwrap();
        assert!(write_atomic_with_mode(&dangling_link, b"data", false).is_err());

        // Directory symlink errors even when mode_0600 == false.
        let sub_dir = target_dir.path().join("real-dir");
        fs::create_dir(&sub_dir).unwrap();
        let dir_link = dir.path().join("dir-link.toml");
        create_test_dir_symlink(&sub_dir, &dir_link).unwrap();
        let dir_err = write_atomic_with_mode(&dir_link, b"data", false).unwrap_err();
        assert!(
            dir_err.to_string().contains("directory"),
            "unexpected error: {dir_err}"
        );
    }
}
