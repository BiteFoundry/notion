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
pub const DEFAULT_GITHUB_REPO: &str = "shinick-dev/notion";
pub const DEFAULT_SERVE_PORT: u16 = 8791;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
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
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            active_workspace: first_non_empty_env(&["NOTION_WORKSPACE"])
                .unwrap_or_else(|| "default".to_string()),
            workspaces: Vec::new(),
            api_base_url: first_non_empty_env(&["NOTION_API_BASE_URL"])
                .unwrap_or_else(|| DEFAULT_NOTION_API_BASE_URL.to_string()),
            oauth_client_id: first_non_empty_env(&["NOTION_OAUTH_CLIENT_ID"]).unwrap_or_default(),
            github_repo: first_non_empty_env(&["NOTION_GITHUB_REPO"])
                .unwrap_or_else(|| DEFAULT_GITHUB_REPO.to_string()),
            notify: true,
            show_sidebar: true,
            page_width: PageWidthMode::Centered,
            inline_images: true,
            show_inline_hrefs: false,
            image_protocol: first_non_empty_env(&["NOTION_IMAGE_PROTOCOL"])
                .and_then(|s| ImageProtocolPreference::parse_str(&s))
                .unwrap_or_default(),
            download_dir: String::new(),
            active_poll_ms: 15_000,
            sync_poll_ms: 60_000,
            cli_cache_ttl_secs: std::env::var("NOTION_CLI_CACHE_SECS")
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(0),
            auto_daemon: std::env::var("NOTION_AUTO_DAEMON")
                .ok()
                .and_then(|s| parse_env_bool(&s))
                .unwrap_or(false),
            density: DensityMode::default(),
            theme: ThemePreset::default(),
        }
    }
}

impl AppConfig {
    pub fn load() -> Self {
        let Ok(mut cfg) = Self::load_from(&config_file_path()) else {
            return Self::default();
        };
        if let Some(ws) = first_non_empty_env(&["NOTION_WORKSPACE"]) {
            cfg.active_workspace = ws;
        }
        if let Some(url) = first_non_empty_env(&["NOTION_API_BASE_URL"]) {
            cfg.api_base_url = url;
        }
        if let Some(cid) = first_non_empty_env(&["NOTION_OAUTH_CLIENT_ID"]) {
            cfg.oauth_client_id = cid;
        }
        if let Some(repo) = first_non_empty_env(&["NOTION_GITHUB_REPO"]) {
            cfg.github_repo = repo;
        }
        if let Ok(ttl) = std::env::var("NOTION_CLI_CACHE_SECS") {
            if let Ok(parsed) = ttl.trim().parse::<u64>() {
                cfg.cli_cache_ttl_secs = parsed;
            }
        }
        if let Ok(ad) = std::env::var("NOTION_AUTO_DAEMON") {
            if let Some(parsed) = parse_env_bool(&ad) {
                cfg.auto_daemon = parsed;
            }
        }
        if let Some(proto) = first_non_empty_env(&["NOTION_IMAGE_PROTOCOL"]) {
            if let Some(parsed) = ImageProtocolPreference::parse_str(&proto) {
                cfg.image_protocol = parsed;
            }
        }
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
        let raw = toml::to_string_pretty(self).context("Failed to serialize config")?;
        write_atomic(path, raw.as_bytes())
    }

    pub fn resolved_download_dir(&self) -> PathBuf {
        if self.download_dir.trim().is_empty() {
            default_download_dir()
        } else {
            expand_tilde(self.download_dir.trim())
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
            self.active_workspace = profile.name;
        }
    }

    pub fn remove_workspace(&mut self, name: &str) -> bool {
        let before = self.workspaces.len();
        self.workspaces
            .retain(|w| !w.name.eq_ignore_ascii_case(name.trim()));
        if self.active_workspace.eq_ignore_ascii_case(name.trim()) {
            self.active_workspace = self
                .workspaces
                .first()
                .map(|w| w.name.clone())
                .unwrap_or_else(|| "default".to_string());
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
        if !override_dir.trim().is_empty() {
            return PathBuf::from(override_dir.trim());
        }
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_DIR_NAME)
}

pub fn data_dir() -> PathBuf {
    if let Ok(override_dir) = std::env::var("NOTION_DATA_DIR") {
        if !override_dir.trim().is_empty() {
            return PathBuf::from(override_dir.trim());
        }
    }
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_DIR_NAME)
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
    ensure_parent(path)?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
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

    if let Err(rename_err) = fs::rename(&tmp, path) {
        if let Err(replace_err) =
            replace_existing_with_backup(&tmp, path, parent, name, pid, seq, rename_err)
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

fn expand_tilde(input: &str) -> PathBuf {
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
}
