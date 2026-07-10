//! Flutter web apps: their "chunks" are listed in `flutter_service_worker.js`.

use std::path::Path;

use regex::Regex;
use url::Url;

use crate::download::download_all;

/// Detect a Flutter web target and return the URL of its
/// `flutter_service_worker.js` (which lists every app resource).
///
/// Handles a direct `flutter_service_worker.js` / `flutter_bootstrap.js` URL,
/// or a page URL whose HTML bootstraps Flutter.
pub fn detect_flutter(
    client: &reqwest::blocking::Client,
    url: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let path = url.split('?').next().unwrap_or("");

    if path.ends_with("flutter_service_worker.js") {
        return Ok(Some(url.to_string()));
    }
    if path.ends_with("flutter_bootstrap.js") || path.ends_with("flutter.js") {
        let sw = Url::parse(url)?.join("flutter_service_worker.js")?;
        return Ok(Some(sw.to_string()));
    }
    // Any other .js/.mjs is not a Flutter entry.
    if path.ends_with(".js") || path.ends_with(".mjs") {
        return Ok(None);
    }

    // Treat as a page: look for Flutter bootstrap markers in the HTML.
    let html = client.get(url).send()?.error_for_status()?.text()?;
    let is_flutter = html.contains("flutter_bootstrap.js")
        || html.contains("_flutter")
        || html.contains("flutter_service_worker.js");
    if is_flutter {
        let sw = Url::parse(url)?.join("flutter_service_worker.js")?;
        Ok(Some(sw.to_string()))
    } else {
        Ok(None)
    }
}

/// Download every resource listed in the Flutter service-worker manifest.
pub fn handle_flutter(
    client: &reqwest::blocking::Client,
    sw_url: &str,
    out_dir: &Path,
    jobs: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let base = Url::parse(sw_url)?;

    // Fetch the service worker; fall back to the bare essentials if absent.
    let sw_body = match client.get(sw_url).send().and_then(|r| r.error_for_status()) {
        Ok(resp) => resp.text()?,
        Err(e) => {
            eprintln!("warning: could not fetch service worker ({e}); falling back to core files");
            let fallback = ["index.html", "flutter_bootstrap.js", "flutter.js", "main.dart.js"]
                .iter()
                .filter_map(|f| base.join(f).ok().map(|u| u.to_string()))
                .collect::<Vec<_>>();
            download_all(client, &fallback, out_dir, jobs);
            return Ok(());
        }
    };

    let mut urls: Vec<String> = parse_flutter_resources(&sw_body)
        .into_iter()
        .filter_map(|p| base.join(&p).ok().map(|u| u.to_string()))
        .collect();

    // Make sure the entry files themselves are captured.
    for f in ["flutter_service_worker.js", "flutter_bootstrap.js", "flutter.js"] {
        if let Ok(u) = base.join(f) {
            urls.push(u.to_string());
        }
    }

    // Dedup preserving order.
    let mut seen = std::collections::HashSet::new();
    urls.retain(|u| seen.insert(u.clone()));

    eprintln!(
        "{} resource(s) listed in the manifest. Downloading into {} ...",
        urls.len(),
        out_dir.display()
    );
    download_all(client, &urls, out_dir, jobs);
    Ok(())
}

/// Extract resource paths from the `RESOURCES = { "path": "hash", ... }` map
/// inside flutter_service_worker.js (keys only).
fn parse_flutter_resources(sw_body: &str) -> Vec<String> {
    // Match "some/path": "deadbeef..." pairs; keep the path (group 1).
    let re = Regex::new(r#""([^"]+)"\s*:\s*"[0-9a-fA-F]{6,}""#).unwrap();
    let mut out: Vec<String> = Vec::new();
    for cap in re.captures_iter(sw_body) {
        let p = cap[1].to_string();
        // Skip the bare "/" root alias (duplicate of index.html).
        if p == "/" {
            continue;
        }
        out.push(p);
    }
    out.sort();
    out.dedup();
    out
}
