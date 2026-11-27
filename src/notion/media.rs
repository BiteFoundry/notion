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

/// Returns `true` if `url` looks like a Notion-hosted expiring AWS S3 signed URL
/// (e.g. `prod-files-secure.s3...` or containing `X-Amz-Expires` / `X-Amz-Signature`).
pub fn is_expiring_notion_s3_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.contains("x-amz-expires")
        || lower.contains("x-amz-signature")
        || lower.contains("prod-files-secure.s3")
}

/// Extracts the media URL from a Notion `image`, `video`, `audio`, `file`, or `pdf` block.
pub fn extract_block_media_url(kind: &NotionBlockKind) -> Option<String> {
    let url = match kind {
        NotionBlockKind::Image { url, .. }
        | NotionBlockKind::Video { url, .. }
        | NotionBlockKind::Audio { url, .. }
        | NotionBlockKind::File { url, .. }
        | NotionBlockKind::Pdf { url, .. } => url.trim(),
        _ => return None,
    };
    (!url.is_empty()).then(|| url.to_string())
}

/// Detects AWS S3 XML error payloads (e.g. `<Error><Code>AccessDenied</Code><Message>Request has expired</Message></Error>`)
/// so expired signed URL responses are never cached or treated as valid media.
pub fn is_s3_error_xml(bytes: &[u8]) -> bool {
    let sample = &bytes[..bytes.len().min(4096)];
    let text = String::from_utf8_lossy(sample);
    let trimmed = text.trim_start_matches('\u{feff}').trim_start();
    let lower = trimmed.to_ascii_lowercase();
    (lower.starts_with("<?xml") || lower.starts_with("<error"))
        && (lower.contains("<error>")
            || (lower.contains("<code") && lower.contains("accessdenied"))
            || lower.contains("request has expired"))
}

/// Detects genuine SVG documents (including those preceded by an `<?xml ...?>` declaration).
pub fn looks_like_svg_document(bytes: &[u8]) -> bool {
    let sample = &bytes[..bytes.len().min(4096)];
    let text = String::from_utf8_lossy(sample);
    let trimmed = text.trim_start_matches('\u{feff}').trim_start();
    let lower = trimmed.to_ascii_lowercase();
    lower.starts_with("<svg")
        || (lower.starts_with("<?xml") && lower.contains("<svg"))
}

/// Detects genuine XML documents (excluding HTML and SVG).
pub fn looks_like_xml_bytes(bytes: &[u8]) -> bool {
    if looks_like_svg_document(bytes) {
        return false;
    }
    let sample = &bytes[..bytes.len().min(4096)];
    let text = String::from_utf8_lossy(sample);
    let trimmed = text.trim_start_matches('\u{feff}').trim_start();
    let lower = trimmed.to_ascii_lowercase();
    lower.starts_with("<?xml") && !lower.contains("<!doctype html") && !lower.contains("<html")
}

/// Fetches raw bytes for a Notion media URL, using an on-disk SHA-256 cache under `data_dir()/media_cache`.
pub async fn fetch_media_bytes_cached(
    http: &reqwest::Client,
    url: &str,
) -> Result<(Vec<u8>, String)> {
    fetch_media_bytes_cached_in_dir(http, url, &media_cache_dir()).await
}

/// Same as `fetch_media_bytes_cached` with an explicit cache directory (also used by unit tests).
pub async fn fetch_media_bytes_cached_in_dir(
    http: &reqwest::Client,
    url: &str,
    cache_dir: &Path,
) -> Result<(Vec<u8>, String)> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        bail!("Media URL is empty");
    }
    let key = cache_key_for_url(trimmed);
    let data_path = cache_dir.join(format!("{key}.bin"));
    let meta_path = cache_dir.join(format!("{key}.mime"));

    if data_path.exists() {
        if let Ok(bytes) = fs::read(&data_path) {
            if is_s3_error_xml(&bytes) {
                let _ = fs::remove_file(&data_path);
                let _ = fs::remove_file(&meta_path);
            } else {
                let cached_mime = fs::read_to_string(&meta_path)
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                let mime = resolve_mime(cached_mime.as_deref(), trimmed, &bytes);
                return Ok((bytes, mime));
            }
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
    if is_s3_error_xml(&bytes) {
        bail!("Media download returned AWS S3 XML error (expired or denied): {trimmed}");
    }
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
    let is_image = is_safe_raster_image(Some(&mime_type), Some(&file_name), Some(&bytes))
        || sniff_content_kind(Some(&mime_type), Some(&file_name), Some(&bytes)) == ContentKind::Svg;

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

/// Longest file name written, in bytes; most file systems allow 255.
const MAX_FILE_NAME_BYTES: usize = 200;

/// Makes a downloaded file name safe on every OS. ASCII keeps only letters, digits, `.`, `-`,
/// `_` and space; other characters are kept (so Korean, Japanese and Chinese names survive)
/// except controls, bidirectional overrides, line separators and full-width slashes.
fn sanitize_filename(raw: &str, mime: &str) -> String {
    let keep = |c: char| {
        if c.is_ascii() {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ')
        } else {
            !(c.is_control()
                || matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
                || matches!(c, '\u{2028}' | '\u{2029}' | '\u{FF0F}' | '\u{FF3C}'))
        }
    };
    let cleaned: String = raw.chars().map(|c| if keep(c) { c } else { '_' }).collect();
    // Windows rejects names that end in a dot or a space.
    let cleaned = cleaned.trim().trim_end_matches(['.', ' ']).to_string();
    let mut name = if cleaned.is_empty() {
        "notion-media".to_string()
    } else {
        cleaned
    };
    // Windows device names are reserved with any extension; keep synced folders portable.
    let stem = name.split('.').next().unwrap_or_default().trim_end();
    let reserved = matches!(stem.to_ascii_uppercase().as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && stem.is_ascii()
            && ["COM", "LPT"].iter().any(|p| stem[..3].eq_ignore_ascii_case(p))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'));
    if reserved {
        name.insert(0, '_');
    }
    if !name.contains('.') {
        name.push_str(ext_for_mime(mime));
    }
    if name.len() > MAX_FILE_NAME_BYTES {
        let ext = name
            .rfind('.')
            .map(|dot| name[dot..].to_string())
            .filter(|ext| ext.len() <= 20)
            .unwrap_or_default();
        let mut cut = MAX_FILE_NAME_BYTES - ext.len();
        while !name.is_char_boundary(cut) {
            cut -= 1;
        }
        name = format!("{}{ext}", name[..cut].trim_end_matches(['.', ' ']));
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
    dir.join(format!("{stem}-{}{ext}", random_uuid_v4()))
}

fn random_uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        u16::from_be_bytes([bytes[4], bytes[5]]),
        u16::from_be_bytes([bytes[6], bytes[7]]),
        u16::from_be_bytes([bytes[8], bytes[9]]),
        u64::from_be_bytes([
            0, 0, bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
        ]),
    )
}

fn resolve_mime(header_mime: Option<&str>, url: &str, bytes: &[u8]) -> String {
    let hint = header_mime
        .map(str::trim)
        .filter(|m| !m.is_empty() && !m.eq_ignore_ascii_case("application/octet-stream"));
    if looks_like_svg_document(bytes) {
        return "image/svg+xml".to_string();
    }
    if looks_like_xml_bytes(bytes) {
        if let Some(h) = hint {
            let lower = h.to_ascii_lowercase();
            if lower == "text/xml" || lower == "application/xml" || lower.ends_with("+xml") {
                return lower;
            }
        }
        return "application/xml".to_string();
    }
    match sniff_content_kind(hint, Some(url), Some(bytes)) {
        ContentKind::RasterImage => {
            if url_path_ends_with(url, ".svg") {
                "image/svg+xml".to_string()
            } else if url_path_ends_with(url, ".xml") {
                "application/xml".to_string()
            } else {
                raster_mime_from_bytes_or_hint(bytes, hint, url)
            }
        }
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
            if url_path_ends_with(url, ".xml") {
                "application/xml".to_string()
            } else if url_path_ends_with(url, ".svg") {
                "image/svg+xml".to_string()
            } else if url_path_ends_with(url, ".json") {
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
            if guessed == "image/svg+xml" {
                guessed
            } else if guessed.starts_with("image/") {
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
    if url_path_ends_with(url, ".svg") {
        return "image/svg+xml".to_string();
    }
    if url_path_ends_with(url, ".xml") {
        return "application/xml".to_string();
    }
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
        "application/xml" | "text/xml" => ".xml",
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
    fn sanitize_filename_keeps_cjk_names_and_ascii_rules() {
        let pdf = "application/pdf";
        assert_eq!(sanitize_filename("2026년_회의록.pdf", pdf), "2026년_회의록.pdf");
        assert_eq!(sanitize_filename("議事録 最終.docx", pdf), "議事録 最終.docx");
        assert_eq!(sanitize_filename("report (1).pdf", pdf), "report _1_.pdf");
        assert_eq!(sanitize_filename("a/b\\c:d.png", pdf), "a_b_c_d.png");
        assert_eq!(sanitize_filename("evil\u{202E}gpj.exe", pdf), "evil_gpj.exe");
        assert_eq!(sanitize_filename("line\u{2028}break\u{FF0F}x.txt", pdf), "line_break_x.txt");
        assert_eq!(sanitize_filename("CON.txt", pdf), "_CON.txt");
        assert_eq!(sanitize_filename("nul", "image/png"), "_nul.png");
        assert_eq!(sanitize_filename("com7.log", pdf), "_com7.log");
        assert_eq!(sanitize_filename("COM0.log", pdf), "COM0.log");
        assert_eq!(sanitize_filename("console.txt", pdf), "console.txt");
        assert_eq!(sanitize_filename("a가.txt", pdf), "a가.txt", "a 4-byte non-ASCII stem is not a device name");
        assert_eq!(sanitize_filename("name. ", "image/png"), "name.png");
        assert_eq!(sanitize_filename("", "image/png"), "notion-media.png");
        let long = format!("{}.pdf", "가".repeat(300));
        let cut = sanitize_filename(&long, pdf);
        assert!(cut.len() <= MAX_FILE_NAME_BYTES, "{}", cut.len());
        assert!(cut.ends_with(".pdf"));
        assert!(cut.starts_with("가가"));
    }

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

        // Genuine XML with <?xml ...?> declaration and XML-prefixed SVG must never be misclassified as PNG
        let xml_svg = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?><svg xmlns=\"http://www.w3.org/2000/svg\"><circle r=\"5\"/></svg>";
        assert_eq!(
            detect_mime(xml_svg, "https://example.com/icon.png"),
            "image/svg+xml"
        );
        let xml_doc = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?><catalog><item>Notion</item></catalog>";
        assert_eq!(
            detect_mime(xml_doc, "https://example.com/feed.png"),
            "application/xml"
        );
        assert_eq!(
            resolve_mime(Some("text/xml"), "https://example.com/feed.xml", xml_doc),
            "text/xml"
        );
        assert_eq!(
            guess_mime_from_url("https://example.com/feed.xml?v=1"),
            "application/xml"
        );
        assert_eq!(ext_for_mime("application/xml"), ".xml");
        assert_eq!(ext_for_mime("text/xml"), ".xml");
    }

    #[test]
    fn test_is_expiring_notion_s3_url_and_s3_error_xml_detection() {
        assert!(is_expiring_notion_s3_url(
            "https://prod-files-secure.s3.us-west-2.amazonaws.com/ws/blk/shot.png?X-Amz-Expires=3600&X-Amz-Signature=abc"
        ));
        assert!(is_expiring_notion_s3_url(
            "https://custom-bucket.s3.amazonaws.com/file.png?X-Amz-Signature=deadbeef"
        ));
        assert!(is_expiring_notion_s3_url(
            "https://prod-files-secure.s3.us-west-2.amazonaws.com/ws/blk/shot.png"
        ));
        assert!(!is_expiring_notion_s3_url("https://example.com/static/logo.png"));

        let s3_err = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>AccessDenied</Code><Message>Request has expired</Message></Error>";
        assert!(is_s3_error_xml(s3_err));

        let benign_xml = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?><feed><title>Updates</title></feed>";
        assert!(!is_s3_error_xml(benign_xml));
    }

    #[tokio::test]
    async fn test_fetch_media_bytes_cached_evicts_poisoned_s3_error_xml_and_never_caches_s3_errors() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        use axum::{http::header, routing::get, Router};
        use tokio::net::TcpListener;

        let valid_png: Vec<u8> = {
            let mut buf = Vec::new();
            let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([10, 180, 90, 255]));
            image::DynamicImage::ImageRgba8(img)
                .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
                .unwrap();
            buf
        };
        let valid_png_clone = valid_png.clone();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_good = hits.clone();

        let s3_err_xml = "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>AccessDenied</Code><Message>Request has expired</Message></Error>";

        let router = Router::new()
            .route(
                "/shot.png",
                get(move || {
                    let png = valid_png_clone.clone();
                    let h = hits_good.clone();
                    async move {
                        h.fetch_add(1, Ordering::SeqCst);
                        (
                            axum::http::StatusCode::OK,
                            [(header::CONTENT_TYPE, "image/png")],
                            png,
                        )
                    }
                }),
            )
            .route(
                "/poison.png",
                get(move || async move {
                    (
                        axum::http::StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/xml")],
                        s3_err_xml,
                    )
                }),
            );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let tmp = tempfile::tempdir().unwrap();
        let http = reqwest::Client::new();

        // 1. Pre-seed cache with poisoned S3 <Error> XML -> must delete poisoned file and re-fetch real PNG!
        let good_url = format!("http://{addr}/shot.png");
        let key = cache_key_for_url(&good_url);
        let data_path = tmp.path().join(format!("{key}.bin"));
        let meta_path = tmp.path().join(format!("{key}.mime"));
        fs::write(&data_path, s3_err_xml.as_bytes()).unwrap();
        fs::write(&meta_path, b"image/png").unwrap();

        let (bytes, mime) = fetch_media_bytes_cached_in_dir(&http, &good_url, tmp.path())
            .await
            .unwrap();
        assert_eq!(bytes, valid_png);
        assert_eq!(mime, "image/png");
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // 2. Network returning S3 <Error> XML must fail and NEVER write to cache!
        let poison_url = format!("http://{addr}/poison.png");
        let poison_key = cache_key_for_url(&poison_url);
        let err = fetch_media_bytes_cached_in_dir(&http, &poison_url, tmp.path())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("S3 XML error"));
        assert!(!tmp.path().join(format!("{poison_key}.bin")).exists());

        server.abort();
    }
}
