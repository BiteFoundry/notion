//! Media download & local cache helper for Notion-hosted (`file`) and external (`external`) image/file URLs.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use reqwest::header::CONTENT_TYPE;
use sha2::{Digest, Sha256};

use crate::{
    config::{ensure_parent, media_cache_dir, AppConfig},
    img_preview::is_image_mime_or_name,
    models::{DownloadedMedia, NotionBlockKind},
    notion::NotionClient,
};

/// Fetches raw bytes for a Notion media URL, using an on-disk SHA-256 cache under `data_dir()/media_cache`.
pub async fn fetch_media_bytes_cached(
    http: &reqwest::Client,
    url: &str,
) -> Result<(Vec<u8>, String)> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        bail!("Media URL is empty");
    }
    let key = cache_key_for_url(trimmed);
    let cache_dir = media_cache_dir();
    let data_path = cache_dir.join(format!("{key}.bin"));
    let meta_path = cache_dir.join(format!("{key}.mime"));

    if data_path.exists() {
        if let Ok(bytes) = fs::read(&data_path) {
            let mime = fs::read_to_string(&meta_path)
                .unwrap_or_else(|_| guess_mime_from_url(trimmed))
                .trim()
                .to_string();
            return Ok((bytes, mime));
        }
    }

    let resp = http
        .get(trimmed)
        .send()
        .await
        .with_context(|| format!("Failed to download media URL: {trimmed}"))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("Media download failed (HTTP {status}): {trimmed}");
    }
    let header_mime = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(';').next().unwrap_or(s).trim().to_string());
    let bytes = resp
        .bytes()
        .await
        .context("Failed to read downloaded media bytes")?
        .to_vec();
    let mime = header_mime
        .filter(|m| !m.is_empty() && m != "application/octet-stream")
        .unwrap_or_else(|| detect_mime(&bytes, trimmed));

    if ensure_parent(&data_path).is_ok() {
        let _ = fs::write(&data_path, &bytes);
        let _ = fs::write(&meta_path, mime.as_bytes());
    }

    Ok((bytes, mime))
}

/// Downloads a URL or Notion block ID (`image`, `file`, `pdf`) into `out_dir` (default `config.resolved_download_dir()`).
pub async fn download_media_target(
    config: &AppConfig,
    client: &NotionClient,
    target: &str,
    out_dir: Option<&Path>,
) -> Result<DownloadedMedia> {
    let trimmed = target.trim();
    let (url, suggested_name) = if trimmed.starts_with("http://") || trimmed.starts_with("https://")
    {
        (trimmed.to_string(), None)
    } else {
        let blk = client.get_block(trimmed, "").await?;
        match blk.kind {
            NotionBlockKind::Image { url, .. } if !url.is_empty() => (url, None),
            NotionBlockKind::Video { url, .. } if !url.is_empty() => (url, Some("video.mp4".into())),
            NotionBlockKind::Audio { url, .. } if !url.is_empty() => (url, Some("audio.mp3".into())),
            NotionBlockKind::File { url, name, .. } if !url.is_empty() => (url, name),
            NotionBlockKind::Pdf { url, .. } if !url.is_empty() => (url, Some("document.pdf".into())),
            other => bail!("Block `{trimmed}` is not a downloadable image/file/pdf block ({other:?})"),
        }
    };

    let (bytes, mime_type) = fetch_media_bytes_cached(client.http_client(), &url).await?;
    let dir = out_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| config.resolved_download_dir());
    fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create download directory: {}", dir.display()))?;

    let base_name = suggested_name
        .filter(|s| !s.trim().is_empty())
        .or_else(|| file_name_from_url(&url))
        .unwrap_or_else(|| format!("notion-media{}", ext_for_mime(&mime_type)));
    let safe_name = sanitize_filename(&base_name, &mime_type);
    let dest = unique_dest_path(&dir, &safe_name);

    fs::write(&dest, &bytes)
        .with_context(|| format!("Failed to write downloaded file: {}", dest.display()))?;

    let file_name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&safe_name)
        .to_string();
    let is_image = is_image_mime_or_name(&mime_type, Some(&file_name));

    Ok(DownloadedMedia {
        ok: true,
        path: dest.to_string_lossy().into_owned(),
        file_name,
        mime_type,
        size_bytes: bytes.len() as u64,
        is_image,
    })
}

fn cache_key_for_url(url: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(url.as_bytes());
    hex::encode(hasher.finalize())
}

fn file_name_from_url(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let seg = parsed.path_segments()?.next_back()?;
    let decoded = urlencoding::decode(seg).ok()?.into_owned();
    let trimmed = decoded.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn sanitize_filename(raw: &str, mime: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim()
        .to_string();
    let mut name = if cleaned.is_empty() {
        "notion-media".to_string()
    } else {
        cleaned
    };
    if !name.contains('.') {
        name.push_str(ext_for_mime(mime));
    }
    name
}

fn unique_dest_path(dir: &Path, file_name: &str) -> PathBuf {
    let candidate = dir.join(file_name);
    if !candidate.exists() {
        return candidate;
    }
    let stem = Path::new(file_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("notion-media");
    let ext = Path::new(file_name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    for idx in 1..1000 {
        let next = dir.join(format!("{stem}-{idx}{ext}"));
        if !next.exists() {
            return next;
        }
    }
    dir.join(format!("{stem}-{}{ext}", uuid::Uuid::new_v4()))
}

fn detect_mime(bytes: &[u8], url: &str) -> String {
    if let Ok(fmt) = image::guess_format(bytes) {
        return match fmt {
            image::ImageFormat::Png => "image/png".to_string(),
            image::ImageFormat::Jpeg => "image/jpeg".to_string(),
            image::ImageFormat::Gif => "image/gif".to_string(),
            image::ImageFormat::WebP => "image/webp".to_string(),
            _ => "application/octet-stream".to_string(),
        };
    }
    if bytes.starts_with(b"%PDF") {
        return "application/pdf".to_string();
    }
    guess_mime_from_url(url)
}

fn guess_mime_from_url(url: &str) -> String {
    let lower = url.split('?').next().unwrap_or(url).to_ascii_lowercase();
    if lower.ends_with(".png") {
        "image/png".to_string()
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg".to_string()
    } else if lower.ends_with(".gif") {
        "image/gif".to_string()
    } else if lower.ends_with(".webp") {
        "image/webp".to_string()
    } else if lower.ends_with(".pdf") {
        "application/pdf".to_string()
    } else {
        "application/octet-stream".to_string()
    }
}

fn ext_for_mime(mime: &str) -> &'static str {
    match mime.trim().to_ascii_lowercase().as_str() {
        "image/png" => ".png",
        "image/jpeg" | "image/jpg" => ".jpg",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        "application/pdf" => ".pdf",
        "text/plain" => ".txt",
        "text/markdown" => ".md",
        "application/json" => ".json",
        _ => ".bin",
    }
}
