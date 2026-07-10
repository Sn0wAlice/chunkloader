//! Entry detection, full page-asset scanning, and Next.js manifest discovery.

use regex::Regex;
use url::Url;

use crate::chunks::{parse_chunks, parse_runtime_maps};
use crate::download::looks_like_html_str;

/// Script-src patterns used to auto-detect the entry file on a page,
/// in priority order (ported verbatim from popup.js). The `runtime~` variant is
/// CRA's external webpack runtime (when not inlined into the HTML) and is the
/// richest entry, so it sits near the top.
pub const ENTRY_PATTERNS: &[&str] = &[
    r"_buildManifest\.js(\?.*)?$",
    r"runtime~[\w.]+\.js(\?.*)?$",
    r"main\.\w+(\.chunk)?\.js(\?.*)?$",
    r"main-\w+(\.chunk)?\.js(\?.*)?$",
    r"runtime\.\w+(\.chunk)?\.js(\?.*)?$",
    r"runtime-\w+(\.chunk)?\.js(\?.*)?$",
    r"webpack-runtime-\w+\.js(\?.*)?$",
    r"app-\w+\.js(\?.*)?$",
    r"app\.\w+(\.chunk)?\.js(\?.*)?$",
    r"\w+\.modern\.js(\?.*)?$",
    // Native ESM bundles (Framer / rolldown / rollup, Vite, ...): treat any
    // module entry as a starting point and crawl its import graph.
    r"\.mjs(\?.*)?$",
];

/// Everything discovered from the target: the entry script(s) to parse, plus the
/// raw page assets used for fallbacks (all `<script>` srcs, preloaded scripts and
/// stylesheets) and the page HTML/base for manifest discovery.
pub struct Target {
    pub entries: Vec<String>,
    /// Same-host `<script src>` + preloaded / `modulepreload` scripts.
    pub page_scripts: Vec<String>,
    /// Same-host stylesheets (`<link rel="stylesheet">` / preloaded styles).
    pub page_styles: Vec<String>,
    /// Chunk URLs resolved from a webpack runtime inlined into the page HTML.
    pub runtime_chunks: Vec<String>,
    /// Assets listed in a CRA/webpack `asset-manifest.json` (JS, CSS, media, maps).
    pub manifest_assets: Vec<String>,
    /// True when the input was a page we scanned (vs. a direct JS/ESM URL).
    pub from_page: bool,
    /// Raw HTML of the scanned page (empty for direct JS URLs).
    pub html: String,
    /// Base URL of the scanned page, for resolving manifests.
    pub base_url: Option<Url>,
}

/// Decide whether `url` is already a JS entry, otherwise fetch the page HTML and
/// auto-detect entry script(s) plus every downloadable asset it references.
pub fn scan_target(
    client: &reqwest::blocking::Client,
    url: &str,
    all_entries: bool,
) -> Result<Target, Box<dyn std::error::Error>> {
    let path = url.split('?').next().unwrap_or("");
    let looks_like_js = ENTRY_PATTERNS
        .iter()
        .any(|p| Regex::new(p).unwrap().is_match(url))
        || path.ends_with(".js")
        || path.ends_with(".mjs");

    if looks_like_js {
        return Ok(Target {
            entries: vec![url.to_string()],
            page_scripts: Vec::new(),
            page_styles: Vec::new(),
            runtime_chunks: Vec::new(),
            manifest_assets: Vec::new(),
            from_page: false,
            html: String::new(),
            base_url: None,
        });
    }

    // Fetch the page and scan for script tags, preloads and stylesheets.
    let html = client.get(url).send()?.error_for_status()?.text()?;
    let base = Url::parse(url)?;
    let page_host = base.host_str().map(String::from);

    // Every <script src>, resolved to an absolute URL (used for entry matching).
    let src_re = Regex::new(r#"<script[^>]+src=["']([^"']+)["']"#).unwrap();
    let mut srcs: Vec<String> = Vec::new();
    for cap in src_re.captures_iter(&html) {
        if let Ok(abs) = base.join(&cap[1]) {
            push_unique(&mut srcs, abs.to_string());
        }
    }

    // Same-host <script> assets feed the fallback set.
    let mut page_scripts: Vec<String> = Vec::new();
    for s in &srcs {
        if same_host(s, &page_host) {
            push_unique(&mut page_scripts, s.clone());
        }
    }

    // <link> tags: preloaded / module-preloaded scripts and stylesheets.
    let mut page_styles: Vec<String> = Vec::new();
    let link_re = Regex::new(r#"<link\b[^>]*>"#).unwrap();
    for m in link_re.find_iter(&html) {
        let tag = m.as_str();
        let href = match attr(tag, "href") {
            Some(h) => h,
            None => continue,
        };
        let abs = match base.join(&href) {
            Ok(a) => a.to_string(),
            Err(_) => continue,
        };
        if !same_host(&abs, &page_host) {
            continue;
        }
        let rel = attr(tag, "rel").unwrap_or_default().to_lowercase();
        let as_attr = attr(tag, "as").unwrap_or_default().to_lowercase();
        let asset_path = abs.split('?').next().unwrap_or("");
        let is_style = rel == "stylesheet"
            || ((rel == "preload" || rel == "prefetch") && as_attr == "style")
            || asset_path.ends_with(".css");
        let is_script = rel == "modulepreload"
            || ((rel == "preload" || rel == "prefetch") && as_attr == "script")
            || asset_path.ends_with(".js")
            || asset_path.ends_with(".mjs");
        if is_style {
            push_unique(&mut page_styles, abs);
        } else if is_script {
            push_unique(&mut page_scripts, abs);
        }
    }

    // Next.js App Router delivers stylesheet hrefs inside the inline RSC/flight
    // payload rather than as <link> tags — sweep the raw HTML for same-host
    // `.css` paths so they're captured too.
    let css_re = Regex::new(r#"(?:https?://[^\s"'\\<>()]+|/[^\s"'\\<>()]+)\.css"#).unwrap();
    for m in css_re.find_iter(&html) {
        if let Ok(abs) = base.join(m.as_str()) {
            let abs = abs.to_string();
            if same_host(&abs, &page_host) {
                push_unique(&mut page_styles, abs);
            }
        }
    }

    // Inline <script> blocks (no src) often hold the webpack runtime with the
    // chunk maps — CRA inlines it into the HTML by default. Parse each and
    // resolve the chunk filenames against the page.
    let mut runtime_chunks: Vec<String> = Vec::new();
    let inline_re = Regex::new(r"(?is)<script\b([^>]*)>(.*?)</script>").unwrap();
    for cap in inline_re.captures_iter(&html) {
        if cap[1].contains("src=") {
            continue; // external script, handled via srcs above
        }
        for rel in parse_runtime_maps(&cap[2]) {
            if let Ok(abs) = base.join(&rel) {
                push_unique(&mut runtime_chunks, abs.to_string());
            }
        }
    }

    // CRA / webpack `asset-manifest.json` (served at the site root) is the
    // authoritative list of every build file — JS, CSS, media, fonts, and maps.
    let manifest_assets = fetch_asset_manifest(client, &base);

    // Match srcs against entry patterns in priority order.
    let mut matches: Vec<String> = Vec::new();
    for pat in ENTRY_PATTERNS {
        let re = Regex::new(pat).unwrap();
        for s in &srcs {
            if re.is_match(s) && !matches.contains(s) {
                matches.push(s.clone());
            }
        }
    }
    let entries = if all_entries {
        matches
    } else {
        matches.into_iter().take(1).collect()
    };

    Ok(Target {
        entries,
        page_scripts,
        page_styles,
        runtime_chunks,
        manifest_assets,
        from_page: true,
        html,
        base_url: Some(base),
    })
}

/// Fetch `<origin>/asset-manifest.json` (CRA / webpack-manifest-plugin) and
/// return every file path it lists, resolved to absolute URLs. Empty when the
/// manifest is absent or unparseable.
fn fetch_asset_manifest(client: &reqwest::blocking::Client, base: &Url) -> Vec<String> {
    let manifest_url = match base.join("/asset-manifest.json") {
        Ok(u) => u,
        Err(_) => return Vec::new(),
    };
    let body = match client
        .get(manifest_url)
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.text())
    {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    let json: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    // Collect string values from the `files` object and/or `entrypoints` array,
    // falling back to any string leaf if neither is present.
    let mut paths: Vec<String> = Vec::new();
    if let Some(files) = json.get("files").and_then(|f| f.as_object()) {
        paths.extend(files.values().filter_map(|v| v.as_str().map(String::from)));
    }
    if let Some(eps) = json.get("entrypoints").and_then(|e| e.as_array()) {
        paths.extend(eps.iter().filter_map(|v| v.as_str().map(String::from)));
    }
    if paths.is_empty() {
        if let Some(obj) = json.as_object() {
            paths.extend(obj.values().filter_map(|v| v.as_str().map(String::from)));
        }
    }

    let mut out: Vec<String> = Vec::new();
    for p in paths {
        if let Ok(abs) = base.join(&p) {
            push_unique(&mut out, abs.to_string());
        }
    }
    out
}

/// Push `s` onto `v` only if not already present (order-preserving dedup).
fn push_unique(v: &mut Vec<String>, s: String) {
    if !v.contains(&s) {
        v.push(s);
    }
}

/// True when `url`'s host matches `host` (both must be present).
fn same_host(url: &str, host: &Option<String>) -> bool {
    match (
        Url::parse(url).ok().and_then(|u| u.host_str().map(String::from)),
        host,
    ) {
        (Some(h), Some(page)) => &h == page,
        _ => false,
    }
}

/// Extract an HTML attribute value (`name="value"` / `name='value'`) from a tag.
fn attr(tag: &str, name: &str) -> Option<String> {
    let re = Regex::new(&format!(
        r#"(?i)\b{}\s*=\s*["']([^"']*)["']"#,
        regex::escape(name)
    ))
    .unwrap();
    re.captures(tag).map(|c| c[1].to_string())
}

/// Best-effort discovery of Next.js build-manifest chunks. Finds the build id in
/// the page and fetches `/_next/static/<id>/_buildManifest.js` (and the SSG
/// manifest), then reuses the standard manifest parser. Yields nothing when no
/// build id is present (e.g. App Router pages that don't expose one).
pub fn discover_next_manifest_chunks(
    client: &reqwest::blocking::Client,
    html: &str,
    base: &Url,
) -> Vec<String> {
    let build_id = Regex::new(r#""buildId"\s*:\s*"([^"]+)""#)
        .unwrap()
        .captures(html)
        .map(|c| c[1].to_string())
        .or_else(|| {
            Regex::new(r#"/_next/static/([^/"']+)/_(?:buildManifest|ssgManifest)\.js"#)
                .unwrap()
                .captures(html)
                .map(|c| c[1].to_string())
        });
    let build_id = match build_id {
        Some(b) if !b.is_empty() && b != "static" && b != "chunks" => b,
        _ => return Vec::new(),
    };

    let mut chunks: Vec<String> = Vec::new();
    for name in ["_buildManifest.js", "_ssgManifest.js"] {
        let manifest_url = match base.join(&format!("/_next/static/{build_id}/{name}")) {
            Ok(u) => u,
            Err(_) => continue,
        };
        let body = match client
            .get(manifest_url.clone())
            .send()
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.text())
        {
            Ok(b) => b,
            Err(_) => continue,
        };
        if looks_like_html_str(&body) {
            continue; // soft-404 served as HTML
        }
        let url_str = manifest_url.as_str();
        let base_path = derive_base_path(url_str);
        chunks.extend(parse_chunks(&body, url_str, &base_path, ".js"));
    }
    chunks
}

/// Base path = URL up to and including the last '/'.
pub fn derive_base_path(url: &str) -> String {
    match url.rfind('/') {
        Some(i) => url[..=i].to_string(),
        None => url.to_string(),
    }
}

/// Ported from popup.js `updateFileExtension`.
pub fn derive_extension(url: &str) -> String {
    let main_js = Regex::new(r"main\.\w+(\.chunk)?\.js(\?.*)?$").unwrap();
    let app_js = Regex::new(r"app-\w+\.js(\?.*)?$").unwrap();
    let dot_chunk = Regex::new(r"\.\w+\.chunk\.js(\?.*)?$").unwrap();
    let dot_modern = Regex::new(r"\w+\.modern\.js(\?.*)?$").unwrap();

    if main_js.is_match(url) || dot_chunk.is_match(url) {
        ".chunk.js".to_string()
    } else if dot_modern.is_match(url) {
        ".modern.js".to_string()
    } else if app_js.is_match(url) {
        ".js".to_string()
    } else {
        ".js".to_string()
    }
}
