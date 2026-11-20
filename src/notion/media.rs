//! Media download & local cache helper for Notion-hosted (`file`) and external (`external`) image/file URLs.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use bitefoundry_html::{is_safe_raster_image, sniff_content_kind, ContentKind};
use reqwest::header::CONTENT_TYPE;
use sha2::{Digest, Sha256};

use crate::{
    config::{ensure_parent, media_cache_dir, AppConfig},
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
            let cached_mime = fs::read_to_string(&meta_path)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            let mime = resolve_mime(cached_mime.as_deref(), trimmed, &bytes);
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
    let mime = resolve_mime(header_mime.as_deref(), trimmed, &bytes);

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
    let is_image = is_safe_raster_image(Some(&mime_type), Some(&file_name), Some(&bytes));

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

fn resolve_mime(header_mime: Option<&str>, url: &str, bytes: &[u8]) -> String {
    let hint = header_mime
        .map(str::trim)
        .filter(|m| !m.is_empty() && !m.eq_ignore_ascii_case("application/octet-stream"));
    match sniff_content_kind(hint, Some(url), Some(bytes)) {
        ContentKind::RasterImage => raster_mime_from_bytes_or_hint(bytes, hint, url),
        ContentKind::Svg => "image/svg+xml".to_string(),
        ContentKind::Html => "text/html".to_string(),
        ContentKind::Pdf => "application/pdf".to_string(),
        ContentKind::Markdown => "text/markdown".to_string(),
        ContentKind::PlainText => {
            if let Some(h) = hint {
                if !h.to_ascii_lowercase().starts_with("image/") {
                    return h.to_ascii_lowercase();
                }
            }
            if url_path_ends_with(url, ".json") {
                "application/json".to_string()
            } else {
                "text/plain".to_string()
            }
        }
        ContentKind::Binary => {
            if let Some(h) = hint {
                if !h.to_ascii_lowercase().starts_with("image/") {
                    return h.to_ascii_lowercase();
                }
            }
            let guessed = guess_mime_from_url(url);
            if guessed.starts_with("image/") {
                "application/octet-stream".to_string()
            } else {
                guessed
            }
        }
    }
}

fn raster_mime_from_bytes_or_hint(bytes: &[u8], hint: Option<&str>, url: &str) -> String {
    if let Ok(fmt) = image::guess_format(bytes) {
        return match fmt {
            image::ImageFormat::Png => "image/png".to_string(),
            image::ImageFormat::Jpeg => "image/jpeg".to_string(),
            image::ImageFormat::Gif => "image/gif".to_string(),
            image::ImageFormat::WebP => "image/webp".to_string(),
            image::ImageFormat::Bmp => "image/bmp".to_string(),
            _ => "image/png".to_string(),
        };
    }
    if bytes.len() >= 2 && &bytes[0..2] == b"BM" {
        return "image/bmp".to_string();
    }
    if let Some(h) = hint {
        let lower = h.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "image/png" | "image/jpeg" | "image/jpg" | "image/gif" | "image/webp" | "image/bmp"
        ) {
            return if lower == "image/jpg" {
                "image/jpeg".to_string()
            } else {
                lower
            };
        }
    }
    guess_mime_from_url(url)
}

fn url_path_ends_with(url: &str, suffix: &str) -> bool {
    url.split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase()
        .ends_with(suffix)
}

pub fn detect_mime(bytes: &[u8], url: &str) -> String {
    resolve_mime(None, url, bytes)
}

pub fn guess_mime_from_url(url: &str) -> String {
    match sniff_content_kind(None, Some(url), None) {
        ContentKind::Html => "text/html".to_string(),
        ContentKind::Svg => "image/svg+xml".to_string(),
        ContentKind::Pdf => "application/pdf".to_string(),
        ContentKind::Markdown => "text/markdown".to_string(),
        ContentKind::PlainText => "text/plain".to_string(),
        ContentKind::RasterImage => {
            if url_path_ends_with(url, ".jpg") || url_path_ends_with(url, ".jpeg") {
                "image/jpeg".to_string()
            } else if url_path_ends_with(url, ".gif") {
                "image/gif".to_string()
            } else if url_path_ends_with(url, ".webp") {
                "image/webp".to_string()
            } else if url_path_ends_with(url, ".bmp") {
                "image/bmp".to_string()
            } else {
                "image/png".to_string()
            }
        }
        ContentKind::Binary => {
            if url_path_ends_with(url, ".json") {
                "application/json".to_string()
            } else {
                "application/octet-stream".to_string()
            }
        }
    }
}

pub fn ext_for_mime(mime: &str) -> &'static str {
    let base = mime
        .split(';')
        .next()
        .unwrap_or(mime)
        .trim()
        .to_ascii_lowercase();
    match base.as_str() {
        "text/html" | "application/xhtml+xml" => ".html",
        "image/svg+xml" => ".svg",
        "image/png" => ".png",
        "image/jpeg" | "image/jpg" => ".jpg",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        "image/bmp" | "image/x-ms-bmp" => ".bmp",
        "application/pdf" => ".pdf",
        "text/plain" => ".txt",
        "text/markdown" | "text/x-markdown" => ".md",
        "application/json" => ".json",
        _ => match sniff_content_kind(Some(&base), None, None) {
            ContentKind::Html => ".html",
            ContentKind::Svg => ".svg",
            ContentKind::Pdf => ".pdf",
            ContentKind::Markdown => ".md",
            ContentKind::PlainText => ".txt",
            _ => ".bin",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_html_and_svg_mime_sniffing_never_misclassified_as_raster_or_bin() {
        let html_bytes = b"<!DOCTYPE html><html><body><h1>Report</h1></body></html>";
        assert_eq!(
            detect_mime(html_bytes, "https://example.com/fake.png"),
            "text/html"
        );
        assert_eq!(
            resolve_mime(Some("image/png"), "https://example.com/fake.png", html_bytes),
            "text/html"
        );
        assert_eq!(
            guess_mime_from_url("https://example.com/report.html?sig=123"),
            "text/html"
        );
        assert_eq!(
            guess_mime_from_url("https://example.com/page.htm"),
            "text/html"
        );
        assert_eq!(
            guess_mime_from_url("https://example.com/doc.xhtml"),
            "text/html"
        );
        assert_eq!(ext_for_mime("text/html"), ".html");
        assert_eq!(ext_for_mime("text/html; charset=utf-8"), ".html");
        assert_eq!(ext_for_mime("application/xhtml+xml"), ".html");

        let svg_bytes = b"<svg xmlns=\"http://www.w3.org/2000/svg\"><rect width=\"10\" height=\"10\"/></svg>";
        assert_eq!(
            detect_mime(svg_bytes, "https://example.com/logo.png"),
            "image/svg+xml"
        );
        assert_eq!(
            guess_mime_from_url("https://example.com/logo.svg"),
            "image/svg+xml"
        );
        assert_eq!(ext_for_mime("image/svg+xml"), ".svg");
    }
}
