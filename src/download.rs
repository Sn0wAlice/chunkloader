//! Parallel downloading and HTML soft-404 detection.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use rayon::prelude::*;
use url::Url;

/// Result of downloading a single URL.
pub enum Outcome {
    Saved(PathBuf),
    /// The URL looked like code but returned an HTML document (soft-404).
    SkippedHtml,
}

pub fn download_all(
    client: &reqwest::blocking::Client,
    urls: &[String],
    out_dir: &Path,
    jobs: usize,
) {
    let ok = AtomicUsize::new(0);
    let skipped = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    let errors: Mutex<Vec<String>> = Mutex::new(Vec::new());

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(jobs.max(1))
        .build()
        .expect("thread pool");

    pool.install(|| {
        urls.par_iter().for_each(|u| {
            match download_one(client, u, out_dir) {
                Ok(Outcome::Saved(path)) => {
                    let n = ok.fetch_add(1, Ordering::Relaxed) + 1;
                    eprintln!("  [ok {n}] {} -> {}", u, path.display());
                }
                Ok(Outcome::SkippedHtml) => {
                    skipped.fetch_add(1, Ordering::Relaxed);
                    eprintln!("  [skip] {u}  (HTML response, not a code asset)");
                }
                Err(e) => {
                    failed.fetch_add(1, Ordering::Relaxed);
                    errors.lock().unwrap().push(format!("{u}  ({e})"));
                }
            }
        });
    });

    let ok = ok.load(Ordering::Relaxed);
    let skipped = skipped.load(Ordering::Relaxed);
    let failed = failed.load(Ordering::Relaxed);
    eprintln!("\nDone: {ok} downloaded, {skipped} skipped, {failed} failed.");
    let errs = errors.into_inner().unwrap();
    if !errs.is_empty() {
        eprintln!("Failures:");
        for e in errs {
            eprintln!("  {e}");
        }
    }
}

pub fn download_one(
    client: &reqwest::blocking::Client,
    url: &str,
    out_dir: &Path,
) -> Result<Outcome, Box<dyn std::error::Error>> {
    let resp = client.get(url).send()?.error_for_status()?;
    let content_type = header_content_type(&resp);
    let bytes = resp.bytes()?;
    if expects_code(url) && looks_like_html(&content_type, &bytes) {
        return Ok(Outcome::SkippedHtml);
    }
    let path = out_dir.join(local_path_for(url));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, &bytes)?;
    Ok(Outcome::Saved(path))
}

/// Map a chunk URL to a relative path on disk, preserving directory structure
/// (query strings are dropped).
pub fn local_path_for(url: &str) -> PathBuf {
    let parsed = Url::parse(url).ok();
    let path = parsed
        .as_ref()
        .map(|p| p.path().to_string())
        .unwrap_or_else(|| url.to_string());
    let trimmed = path.trim_start_matches('/');
    let rel = if trimmed.is_empty() || trimmed.ends_with('/') {
        format!("{trimmed}index.js")
    } else {
        trimmed.to_string()
    };
    // Guard against path traversal in weird URLs.
    let safe: PathBuf = Path::new(&rel)
        .components()
        .filter(|c| !matches!(c, std::path::Component::ParentDir))
        .collect();
    safe
}

/// Read the `Content-Type` header of a response as a lowercase string.
pub fn header_content_type(resp: &reqwest::blocking::Response) -> String {
    resp.headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

/// True when the URL is expected to serve code/data (JS/MJS/CSS/JSON), so an
/// HTML response indicates a soft-404 rather than legitimate content.
pub fn expects_code(url: &str) -> bool {
    let path = url.split('?').next().unwrap_or("");
    path.ends_with(".js")
        || path.ends_with(".mjs")
        || path.ends_with(".css")
        || path.ends_with(".json")
}

/// True when the response headers or body look like an HTML document.
pub fn looks_like_html(content_type: &str, bytes: &[u8]) -> bool {
    if content_type.to_lowercase().contains("text/html") {
        return true;
    }
    let head = &bytes[..bytes.len().min(512)];
    looks_like_html_str(&String::from_utf8_lossy(head))
}

/// True when `body` begins with an HTML doctype / `<html>` root.
pub fn looks_like_html_str(body: &str) -> bool {
    let head = body.trim_start().to_lowercase();
    head.starts_with("<!doctype html") || head.starts_with("<html")
}
