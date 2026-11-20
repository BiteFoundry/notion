//! Authentication & Multi-Workspace credential registry for `@bitefoundry/notion`.
//!
//! Supports two auth modes cleanly:
//! 1. Internal Integration Token (`notion login --token ntn_...` / `NOTION_TOKEN`), validated via `GET /v1/users/me`.
//! 2. Public OAuth 2.0 (`notion login --oauth --client-id <id> --client-secret <secret>`),
//!    using localhost loopback callback + HTTP Basic-Auth exchange at `POST /v1/oauth/token`.
//!
//! Tokens are stored per workspace profile in the OS keyring (`keyring`, service `"notion-cli"`)
//! with automatic `0600` fallback file (`credentials.json`) for headless environments.

use std::{
    collections::HashMap,
    fs,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64_STANDARD, Engine as _};
use rand::RngCore;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::RwLock,
};
use url::Url;

use crate::{
    config::{
        credential_fallback_path, write_atomic_0600, AppConfig, WorkspaceProfile,
        KEYRING_SERVICE_NAME, NOTION_API_VERSION,
    },
    models::NotionUser,
};

const OAUTH_CALLBACK_TIMEOUT_SECS: u64 = 300;
pub const NOTION_INTEGRATIONS_URL: &str = "https://www.notion.so/profile/integrations";

/// Renders a compact, scannable Unicode half-block (`▀`, `▄`, `█`, ` `) QR code for terminal display.
pub fn render_terminal_qr(data: &str) -> Option<String> {
    let code = qrcode::QrCode::new(data.as_bytes()).ok()?;
    let width = code.width();
    let quiet = 1usize;
    let total = width + quiet * 2;

    let is_dark = |x: usize, y: usize| -> bool {
        if x < quiet || y < quiet || x >= width + quiet || y >= width + quiet {
            false
        } else {
            matches!(code[(x - quiet, y - quiet)], qrcode::Color::Dark)
        }
    };

    let mut out = String::new();
    let mut y = 0;
    while y < total {
        out.push_str("  ");
        for x in 0..total {
            let top = is_dark(x, y);
            let bottom = if y + 1 < total { is_dark(x, y + 1) } else { false };
            // Light background (inverted for dark terminals: dark modules = space, light = block)
            let ch = match (top, bottom) {
                (false, false) => '█',
                (false, true) => '▀',
                (true, false) => '▄',
                (true, true) => ' ',
            };
            out.push(ch);
        }
        out.push('\n');
        y += 2;
    }
    Some(out)
}
#[cfg(target_os = "linux")]
const LINUX_KEYRING_TIMEOUT: Duration = Duration::from_millis(800);

static KEYRING_RUNTIME_FAILED: AtomicBool = AtomicBool::new(false);

/// Stored credential record for a single named Notion workspace profile.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredCredential {
    pub workspace_profile: String,
    pub access_token: String,
    pub workspace_id: String,
    pub workspace_name: String,
    pub workspace_icon: Option<String>,
    pub bot_id: Option<String>,
    pub bot_name: Option<String>,
    pub auth_mode: String,
}

impl std::fmt::Debug for StoredCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredCredential")
            .field("workspace_profile", &self.workspace_profile)
            .field("access_token", &"[REDACTED]")
            .field("workspace_id", &self.workspace_id)
            .field("workspace_name", &self.workspace_name)
            .field("bot_id", &self.bot_id)
            .field("auth_mode", &self.auth_mode)
            .finish()
    }
}

#[derive(Debug, Clone, Default)]
pub struct OAuthLoginOptions {
    pub client_id: String,
    pub client_secret: String,
    pub workspace_alias: Option<String>,
    pub port: Option<u16>,
    pub redirect_uri: Option<String>,
    pub no_browser: bool,
    pub no_qr: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CredentialFileStore {
    #[serde(default)]
    workspaces: HashMap<String, StoredCredential>,
}

#[derive(Clone)]
pub struct AuthManager {
    config: Arc<RwLock<AppConfig>>,
    active_workspace_override: Option<String>,
    http: reqwest::Client,
    cached_cred: Arc<RwLock<Option<StoredCredential>>>,
    ephemeral: bool,
}

impl AuthManager {
    pub fn new(config: AppConfig, workspace_override: Option<String>) -> Result<Self> {
        let http = build_auth_http_client()?;
        let active_name = config.effective_workspace_name(workspace_override.as_deref());
        let loaded = if should_disable_keyring() {
            load_credential_from_file(&credential_fallback_path(), &active_name)?
        } else {
            load_credential(&active_name)?
        };
        Ok(Self {
            config: Arc::new(RwLock::new(config)),
            active_workspace_override: workspace_override,
            http,
            cached_cred: Arc::new(RwLock::new(loaded)),
            ephemeral: false,
        })
    }

    pub fn from_token(config: AppConfig, token: impl Into<String>) -> Self {
        let http = build_auth_http_client().unwrap_or_else(|_| reqwest::Client::new());
        let ws = config.effective_workspace_name(None);
        let cred = StoredCredential {
            workspace_profile: ws.clone(),
            access_token: token.into(),
            workspace_id: ws.clone(),
            workspace_name: ws,
            workspace_icon: None,
            bot_id: None,
            bot_name: None,
            auth_mode: "integration".to_string(),
        };
        Self {
            config: Arc::new(RwLock::new(config)),
            active_workspace_override: None,
            http,
            cached_cred: Arc::new(RwLock::new(Some(cred))),
            ephemeral: true,
        }
    }

    pub async fn active_workspace_name(&self) -> String {
        let cfg = self.config.read().await;
        cfg.effective_workspace_name(self.active_workspace_override.as_deref())
    }

    pub async fn active_workspace_id(&self) -> String {
        if let Some(cred) = self.cached_cred.read().await.as_ref() {
            if !cred.workspace_id.trim().is_empty() {
                return cred.workspace_id.clone();
            }
        }
        let cfg = self.config.read().await;
        let name = cfg.effective_workspace_name(self.active_workspace_override.as_deref());
        cfg.find_workspace(&name)
            .map(|p| p.effective_workspace_id().to_string())
            .unwrap_or(name)
    }

    pub async fn current_credential(&self) -> Option<StoredCredential> {
        if let Ok(env_tok) = std::env::var("NOTION_TOKEN") {
            let trimmed = env_tok.trim();
            if !trimmed.is_empty() && !self.ephemeral {
                let ws = self.active_workspace_name().await;
                return Some(StoredCredential {
                    workspace_profile: ws.clone(),
                    access_token: trimmed.to_string(),
                    workspace_id: ws.clone(),
                    workspace_name: ws,
                    workspace_icon: None,
                    bot_id: None,
                    bot_name: None,
                    auth_mode: "env".to_string(),
                });
            }
        }
        if let Some(cred) = self.cached_cred.read().await.clone() {
            return Some(cred);
        }
        if !self.ephemeral {
            let ws = self.active_workspace_name().await;
            if let Ok(Some(cred)) = load_credential(&ws) {
                *self.cached_cred.write().await = Some(cred.clone());
                return Some(cred);
            }
        }
        None
    }

    /// Returns the active Bearer token for Notion REST API calls.
    pub async fn access_token(&self) -> Result<String> {
        if let Some(cred) = self.current_credential().await {
            if !cred.access_token.trim().is_empty() {
                return Ok(cred.access_token);
            }
        }
        bail!(
            "Not logged in to Notion. Run `notion login` (with an internal integration token or `--oauth`) or set `NOTION_TOKEN`."
        );
    }

    /// Switches active workspace in memory (used by TUI Workspace Switcher `Ctrl+S`).
    pub async fn switch_workspace(&self, workspace_name: &str) -> Result<Option<StoredCredential>> {
        let trimmed = workspace_name.trim().to_string();
        if trimmed.is_empty() {
            bail!("Workspace name cannot be empty");
        }
        {
            let mut cfg = self.config.write().await;
            cfg.active_workspace = trimmed.clone();
            if !self.ephemeral {
                let _ = cfg.save();
            }
        }
        let loaded = if self.ephemeral {
            self.cached_cred.read().await.clone()
        } else {
            load_credential(&trimmed)?
        };
        *self.cached_cred.write().await = loaded.clone();
        Ok(loaded)
    }

    /// Validates an Internal Integration Token via `GET /v1/users/me` and stores it in keyring + config.
    pub async fn login_with_token(
        &self,
        token: &str,
        workspace_alias: Option<&str>,
    ) -> Result<(StoredCredential, NotionUser)> {
        let clean_token = token.trim();
        if clean_token.is_empty() {
            bail!("Integration token cannot be empty");
        }

        let base_url = self.config.read().await.effective_api_base_url();
        let me = validate_token_via_users_me(&self.http, &base_url, clean_token).await?;

        let workspace_name = me
            .workspace_name
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| {
                workspace_alias
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("Notion Workspace")
                    .to_string()
            });

        let alias = workspace_alias
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(sanitize_profile_slug)
            .unwrap_or_else(|| {
                let slug = sanitize_profile_slug(&workspace_name);
                if slug.is_empty() {
                    "default".to_string()
                } else {
                    slug
                }
            });

        let workspace_id = if !me.id.trim().is_empty() {
            format!("ws-{}", &me.id.replace('-', "")[..me.id.replace('-', "").len().min(12)])
        } else {
            alias.clone()
        };

        let cred = StoredCredential {
            workspace_profile: alias.clone(),
            access_token: clean_token.to_string(),
            workspace_id: workspace_id.clone(),
            workspace_name: workspace_name.clone(),
            workspace_icon: None,
            bot_id: Some(me.id.clone()),
            bot_name: Some(me.name.clone()),
            auth_mode: "integration".to_string(),
        };

        self.persist_workspace_login(&cred, true).await?;
        Ok((cred, me))
    }

    /// Performs Notion Public OAuth 2.0 authorization via localhost loopback callback + HTTP Basic-Auth token exchange.
    pub async fn login_with_oauth(&self, opts: OAuthLoginOptions) -> Result<StoredCredential> {
        let client_id = opts.client_id.trim().to_string();
        let client_secret = opts.client_secret.trim().to_string();
        if client_id.is_empty() || client_secret.is_empty() {
            bail!("OAuth login requires both `--client-id` and `--client-secret`");
        }

        let port = opts.port.unwrap_or(0);
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .context("Failed to bind localhost OAuth callback listener")?;
        let bound_port = listener.local_addr()?.port();
        let redirect_uri = opts
            .redirect_uri
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("http://127.0.0.1:{bound_port}/callback"));

        let state = generate_random_hex(16);
        let base_url = self.config.read().await.effective_api_base_url();
        let auth_url = build_oauth_authorize_url(&base_url, &client_id, &redirect_uri, &state)?;

        let env_no_browser = std::env::var("NOTION_NO_BROWSER")
            .ok()
            .is_some_and(|v| matches!(v.trim(), "1" | "true" | "yes"));
        let opened = if !opts.no_browser && !env_no_browser {
            open::that(&auth_url).is_ok()
        } else {
            false
        };
        if opened {
            eprintln!("Opened Notion authorization page in your browser.");
        }
        eprintln!("Open this URL to authorize Notion:\n  {auth_url}");
        if !opts.no_qr {
            if let Some(qr) = render_terminal_qr(&auth_url) {
                eprintln!("\nOr scan this QR code:\n{qr}");
            }
        }
        eprintln!("Waiting for OAuth callback on {redirect_uri}...");

        let code = wait_for_oauth_callback(listener, &state).await?;
        let cred = exchange_oauth_code(
            &self.http,
            &base_url,
            &client_id,
            &client_secret,
            &code,
            &redirect_uri,
            opts.workspace_alias.as_deref(),
        )
        .await?;

        self.persist_workspace_login(&cred, true).await?;
        Ok(cred)
    }

    async fn persist_workspace_login(
        &self,
        cred: &StoredCredential,
        make_active: bool,
    ) -> Result<()> {
        if !self.ephemeral {
            save_credential(cred)?;
            let mut cfg = self.config.write().await;
            cfg.upsert_workspace(
                WorkspaceProfile {
                    name: cred.workspace_profile.clone(),
                    workspace_id: cred.workspace_id.clone(),
                    workspace_name: cred.workspace_name.clone(),
                    workspace_icon: cred.workspace_icon.clone(),
                    bot_id: cred.bot_id.clone(),
                    bot_name: cred.bot_name.clone(),
                    auth_mode: cred.auth_mode.clone(),
                    api_base_url: None,
                },
                make_active,
            );
            cfg.save()?;
        }
        *self.cached_cred.write().await = Some(cred.clone());
        Ok(())
    }

    /// Removes stored credentials for the active (or specified) workspace profile.
    pub async fn logout(&self, workspace_name: Option<&str>) -> Result<String> {
        let target = match workspace_name.map(str::trim).filter(|s| !s.is_empty()) {
            Some(name) => name.to_string(),
            None => self.active_workspace_name().await,
        };
        if !self.ephemeral {
            delete_credential(&target)?;
            let mut cfg = self.config.write().await;
            cfg.remove_workspace(&target);
            cfg.save()?;
        }
        *self.cached_cred.write().await = None;
        Ok(target)
    }
}

pub fn build_oauth_authorize_url(
    api_base_url: &str,
    client_id: &str,
    redirect_uri: &str,
    state: &str,
) -> Result<String> {
    let base = api_base_url.trim_end_matches('/');
    let mut url = Url::parse(&format!("{base}/oauth/authorize"))
        .context("Failed to construct Notion OAuth authorize URL")?;
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("response_type", "code")
        .append_pair("owner", "user")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("state", state);
    Ok(url.to_string())
}

pub async fn validate_token_via_users_me(
    http: &reqwest::Client,
    api_base_url: &str,
    token: &str,
) -> Result<NotionUser> {
    let url = format!("{}/users/me", api_base_url.trim_end_matches('/'));
    let resp = http
        .get(&url)
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .header("Notion-Version", NOTION_API_VERSION)
        .send()
        .await
        .with_context(|| format!("Failed to reach Notion API at {url}"))?;
    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .unwrap_or_else(|_| serde_json::json!({}));
    if !status.is_success() {
        let msg = body
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Authentication rejected");
        bail!("Notion token validation failed (HTTP {status}): {msg}");
    }
    Ok(parse_notion_user(&body))
}

pub async fn exchange_oauth_code(
    http: &reqwest::Client,
    api_base_url: &str,
    client_id: &str,
    client_secret: &str,
    code: &str,
    redirect_uri: &str,
    workspace_alias: Option<&str>,
) -> Result<StoredCredential> {
    let url = format!("{}/oauth/token", api_base_url.trim_end_matches('/'));
    let basic = B64_STANDARD.encode(format!("{client_id}:{client_secret}"));
    let payload = serde_json::json!({
        "grant_type": "authorization_code",
        "code": code,
        "redirect_uri": redirect_uri,
    });
    let resp = http
        .post(&url)
        .header(AUTHORIZATION, format!("Basic {basic}"))
        .header("Notion-Version", NOTION_API_VERSION)
        .header(CONTENT_TYPE, "application/json")
        .json(&payload)
        .send()
        .await
        .with_context(|| format!("Failed to exchange Notion OAuth token at {url}"))?;
    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .context("Failed to parse Notion OAuth token response JSON")?;
    if !status.is_success() {
        let msg = body
            .get("error_description")
            .or_else(|| body.get("message"))
            .or_else(|| body.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("OAuth token exchange failed");
        bail!("Notion OAuth exchange failed (HTTP {status}): {msg}");
    }

    let access_token = body
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow!("Notion OAuth response missing `access_token`"))?
        .to_string();
    let workspace_id = body
        .get("workspace_id")
        .and_then(Value::as_str)
        .unwrap_or("default")
        .to_string();
    let workspace_name = body
        .get("workspace_name")
        .and_then(Value::as_str)
        .unwrap_or("Notion Workspace")
        .to_string();
    let workspace_icon = body
        .get("workspace_icon")
        .and_then(Value::as_str)
        .map(str::to_string);
    let bot_id = body
        .get("bot_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let bot_name = body
        .get("owner")
        .and_then(|o| o.get("user"))
        .and_then(|u| u.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let alias = workspace_alias
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(sanitize_profile_slug)
        .unwrap_or_else(|| {
            let slug = sanitize_profile_slug(&workspace_name);
            if slug.is_empty() {
                "default".to_string()
            } else {
                slug
            }
        });

    Ok(StoredCredential {
        workspace_profile: alias,
        access_token,
        workspace_id,
        workspace_name,
        workspace_icon,
        bot_id,
        bot_name,
        auth_mode: "oauth".to_string(),
    })
}

async fn wait_for_oauth_callback(listener: TcpListener, expected_state: &str) -> Result<String> {
    let accept_fut = async {
        let (mut stream, _) = listener
            .accept()
            .await
            .context("Failed to accept OAuth loopback callback")?;
        let mut buf = [0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap_or(0);
        let req_text = String::from_utf8_lossy(&buf[..n]);
        let first_line = req_text.lines().next().unwrap_or("");
        let path = first_line.split_whitespace().nth(1).unwrap_or("/");
        let parsed = Url::parse(&format!("http://127.0.0.1{path}"))
            .context("Failed to parse OAuth callback query")?;

        let mut code = None;
        let mut state = None;
        let mut err_msg = None;
        for (k, v) in parsed.query_pairs() {
            match k.as_ref() {
                "code" => code = Some(v.into_owned()),
                "state" => state = Some(v.into_owned()),
                "error" => err_msg = Some(v.into_owned()),
                _ => {}
            }
        }

        let (html, res) = if let Some(err) = err_msg {
            (
                format!("<html><body><h2>Notion authorization failed</h2><p>{err}</p></body></html>"),
                Err(anyhow!("Notion OAuth authorization rejected: {err}")),
            )
        } else if state.as_deref() != Some(expected_state) {
            (
                "<html><body><h2>State mismatch</h2><p>Please retry login.</p></body></html>".to_string(),
                Err(anyhow!("OAuth CSRF state mismatch")),
            )
        } else if let Some(c) = code {
            (
                "<html><body><h2>Connected to Notion!</h2><p>You may close this tab and return to your terminal.</p></body></html>".to_string(),
                Ok(c),
            )
        } else {
            (
                "<html><body><h2>Missing authorization code</h2></body></html>".to_string(),
                Err(anyhow!("OAuth callback missing `code` parameter")),
            )
        };

        let http_resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            html.len(),
            html
        );
        let _ = stream.write_all(http_resp.as_bytes()).await;
        let _ = stream.flush().await;
        res
    };

    tokio::time::timeout(Duration::from_secs(OAUTH_CALLBACK_TIMEOUT_SECS), accept_fut)
        .await
        .context("Timed out waiting for Notion OAuth callback")?
}

pub fn parse_notion_user(val: &Value) -> NotionUser {
    let id = val
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let name = val
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("Notion User")
        .to_string();
    let user_type = val
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("bot")
        .to_string();
    let avatar_url = val
        .get("avatar_url")
        .and_then(Value::as_str)
        .map(str::to_string);
    let email = val
        .get("person")
        .and_then(|p| p.get("email"))
        .and_then(Value::as_str)
        .or_else(|| {
            val.get("bot")
                .and_then(|b| b.get("owner"))
                .and_then(|o| o.get("user"))
                .and_then(|u| u.get("person"))
                .and_then(|p| p.get("email"))
                .and_then(Value::as_str)
        })
        .map(str::to_string);
    let workspace_name = val
        .get("bot")
        .and_then(|b| b.get("workspace_name"))
        .and_then(Value::as_str)
        .map(str::to_string);
    NotionUser {
        id,
        name,
        user_type,
        avatar_url,
        email,
        workspace_name,
    }
}

pub fn sanitize_profile_slug(raw: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in raw.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

fn generate_random_hex(num_bytes: usize) -> String {
    let mut bytes = vec![0u8; num_bytes];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn build_auth_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(format!("notion-cli/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("Failed to initialize auth HTTP client")
}

fn should_disable_keyring() -> bool {
    if KEYRING_RUNTIME_FAILED.load(Ordering::Relaxed) {
        return true;
    }
    std::env::var("NOTION_DISABLE_KEYRING")
        .ok()
        .is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

fn keyring_account(workspace_profile: &str) -> String {
    format!("workspace:{}", workspace_profile.trim().to_ascii_lowercase())
}

pub fn save_credential(cred: &StoredCredential) -> Result<()> {
    let payload = serde_json::to_string(cred)?;
    if !should_disable_keyring() && try_keyring_set(&keyring_account(&cred.workspace_profile), &payload)
    {
        // Also remove any stale plaintext entry for this workspace if keyring succeeded.
        let _ = remove_credential_from_file(&credential_fallback_path(), &cred.workspace_profile);
        return Ok(());
    }
    save_credential_to_file(&credential_fallback_path(), cred)
}

pub fn load_credential(workspace_profile: &str) -> Result<Option<StoredCredential>> {
    if !should_disable_keyring() {
        if let Some(raw) = try_keyring_get(&keyring_account(workspace_profile)) {
            if let Ok(parsed) = serde_json::from_str::<StoredCredential>(&raw) {
                return Ok(Some(parsed));
            }
        }
    }
    load_credential_from_file(&credential_fallback_path(), workspace_profile)
}

pub fn delete_credential(workspace_profile: &str) -> Result<()> {
    if !should_disable_keyring() {
        try_keyring_delete(&keyring_account(workspace_profile));
    }
    remove_credential_from_file(&credential_fallback_path(), workspace_profile)
}

fn try_keyring_set(account: &str, secret: &str) -> bool {
    let acc = account.to_string();
    let sec = secret.to_string();
    let op = move || -> Result<(), String> {
        let entry = keyring::Entry::new(KEYRING_SERVICE_NAME, &acc).map_err(|e| e.to_string())?;
        entry.set_password(&sec).map_err(|e| e.to_string())
    };
    run_keyring_bounded(op).is_some_and(|r| r.is_ok())
}

fn try_keyring_get(account: &str) -> Option<String> {
    let acc = account.to_string();
    let op = move || -> Result<String, String> {
        let entry = keyring::Entry::new(KEYRING_SERVICE_NAME, &acc).map_err(|e| e.to_string())?;
        entry.get_password().map_err(|e| e.to_string())
    };
    match run_keyring_bounded(op) {
        Some(Ok(val)) => Some(val),
        _ => None,
    }
}

fn try_keyring_delete(account: &str) {
    let acc = account.to_string();
    let op = move || -> Result<(), String> {
        let entry = keyring::Entry::new(KEYRING_SERVICE_NAME, &acc).map_err(|e| e.to_string())?;
        entry.delete_credential().map_err(|e| e.to_string())
    };
    let _ = run_keyring_bounded(op);
}

#[cfg(target_os = "linux")]
fn run_keyring_bounded<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Option<Result<T, String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(LINUX_KEYRING_TIMEOUT) {
        Ok(res) => Some(res),
        Err(_) => {
            KEYRING_RUNTIME_FAILED.store(true, Ordering::Relaxed);
            None
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn run_keyring_bounded<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Option<Result<T, String>> {
    Some(f())
}

fn save_credential_to_file(path: &Path, cred: &StoredCredential) -> Result<()> {
    let mut store = read_cred_file_store(path);
    store
        .workspaces
        .insert(cred.workspace_profile.to_ascii_lowercase(), cred.clone());
    let bytes = serde_json::to_vec_pretty(&store)?;
    write_atomic_0600(path, &bytes)
}

fn load_credential_from_file(path: &Path, workspace_profile: &str) -> Result<Option<StoredCredential>> {
    let store = read_cred_file_store(path);
    let key = workspace_profile.trim().to_ascii_lowercase();
    Ok(store.workspaces.get(&key).cloned())
}

fn remove_credential_from_file(path: &Path, workspace_profile: &str) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let mut store = read_cred_file_store(path);
    let key = workspace_profile.trim().to_ascii_lowercase();
    if store.workspaces.remove(&key).is_some() {
        let bytes = serde_json::to_vec_pretty(&store)?;
        write_atomic_0600(path, &bytes)?;
    }
    Ok(())
}

fn read_cred_file_store(path: &Path) -> CredentialFileStore {
    let Ok(raw) = fs::read_to_string(path) else {
        return CredentialFileStore::default();
    };
    serde_json::from_str::<CredentialFileStore>(&raw).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EnvVarGuard, TEST_ENV_LOCK};
    use axum::{
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
        routing::{get, post},
        Json, Router,
    };

    #[tokio::test]
    async fn test_integration_token_and_oauth_exchange_against_mock_server() {
        let _lock = TEST_ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let cfg_dir = tmp.path().to_string_lossy().to_string();
        let _guard = EnvVarGuard::set(&[
            ("NOTION_CONFIG_DIR", Some(&cfg_dir)),
            ("NOTION_DISABLE_KEYRING", Some("1")),
            ("NOTION_TOKEN", None),
        ]);

        let app = Router::new()
            .route(
                "/v1/users/me",
                get(|headers: HeaderMap| async move {
                    let auth = headers
                        .get(AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    let ver = headers
                        .get("Notion-Version")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    if auth != "Bearer ntn_test_secret" || ver != NOTION_API_VERSION {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(serde_json::json!({ "message": "Invalid token" })),
                        )
                            .into_response();
                    }
                    Json(serde_json::json!({
                        "object": "user",
                        "id": "bot-12345678-aaaa",
                        "name": "Engineering Bot",
                        "type": "bot",
                        "bot": { "workspace_name": "Acme Engineering" }
                    }))
                    .into_response()
                }),
            )
            .route(
                "/v1/oauth/token",
                post(|headers: HeaderMap, Json(body): Json<Value>| async move {
                    let auth = headers
                        .get(AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    let expected = format!("Basic {}", B64_STANDARD.encode("cid-1:csec-2"));
                    if auth != expected {
                        return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error":"invalid_client"}))).into_response();
                    }
                    assert_eq!(body["code"], "oauth-code-99");
                    Json(serde_json::json!({
                        "access_token": "ntn_oauth_access_token",
                        "workspace_id": "ws-oauth-77",
                        "workspace_name": "Design Studio",
                        "workspace_icon": "🎨",
                        "bot_id": "bot-oauth-1",
                        "owner": { "user": { "name": "Alice" } }
                    }))
                    .into_response()
                }),
            );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut cfg = AppConfig::default();
        cfg.api_base_url = format!("http://{addr}/v1");
        let auth = AuthManager::new(cfg.clone(), None).unwrap();

        let (cred, user) = auth
            .login_with_token("ntn_test_secret", Some("eng"))
            .await
            .unwrap();
        assert_eq!(cred.workspace_profile, "eng");
        assert_eq!(cred.workspace_name, "Acme Engineering");
        assert_eq!(user.name, "Engineering Bot");
        assert_eq!(auth.access_token().await.unwrap(), "ntn_test_secret");

        let oauth_cred = exchange_oauth_code(
            &reqwest::Client::new(),
            &format!("http://{addr}/v1"),
            "cid-1",
            "csec-2",
            "oauth-code-99",
            "http://127.0.0.1:9999/callback",
            Some("design"),
        )
        .await
        .unwrap();
        assert_eq!(oauth_cred.workspace_profile, "design");
        assert_eq!(oauth_cred.workspace_id, "ws-oauth-77");
        assert_eq!(oauth_cred.workspace_icon.as_deref(), Some("🎨"));
        assert_eq!(oauth_cred.auth_mode, "oauth");

        server.abort();
    }
}
