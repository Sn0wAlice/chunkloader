//! chunkloader — Rust port of the "Chunk Loader" browser extension.
//!
//! Given a page/domain URL or a direct JS entry URL (webpack runtime, Next.js
//! _buildManifest, "modern" chunk file, ...), it discovers every referenced JS
//! chunk and downloads them into a local folder for offline analysis.
//!
//! Parsing strategies mirror the extension's `content.js`:
//!   1. Next.js `self.__BUILD_MANIFEST = function(...){...}(...)`  (JS eval)
//!   2. Next.js `self.__BUILD_MANIFEST = {...}`                    (JS eval)
//!   3. "modern" chunks: `return o.p + "" + {id: "name", ...}`
//!   4. webpack runtime: two `{id:"name"}` maps combined as `name1-name2`
//!   5. standard webpack chunks: `{id:"hash"}` -> `id.hash<ext>`

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use clap::Parser;
use rayon::prelude::*;
use regex::Regex;
use url::Url;

/// Script-src patterns used to auto-detect the entry file on a page,
/// in priority order (ported verbatim from popup.js).
const ENTRY_PATTERNS: &[&str] = &[
    r"_buildManifest\.js(\?.*)?$",
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

#[derive(Parser, Debug)]
#[command(
    name = "chunkloader",
    about = "Dump webpack / Next.js JS chunks from a site into a local folder for analysis"
)]
struct Args {
    /// Page/domain URL (auto-detect the entry) or a direct JS entry URL.
    url: String,

    /// Output directory (default: ./dump/<host>).
    #[arg(short, long)]
    out: Option<PathBuf>,

    /// Override the base path used to resolve chunks (default: derived from the entry URL).
    #[arg(short, long)]
    base: Option<String>,

    /// Override the chunk file extension (default: derived from the entry URL).
    #[arg(short, long)]
    ext: Option<String>,

    /// Only detect and print the entry URL(s); do not download anything.
    #[arg(long)]
    entry_only: bool,

    /// When auto-detecting from a page, process every matched entry, not just the best one.
    #[arg(long)]
    all_entries: bool,

    /// Number of parallel downloads.
    #[arg(short, long, default_value_t = 8)]
    jobs: usize,

    /// Accept invalid TLS certificates.
    #[arg(long)]
    insecure: bool,

    /// User-Agent header to send.
    #[arg(
        long,
        default_value = "Mozilla/5.0 (compatible; chunkloader/0.1; +https://github.com/)"
    )]
    user_agent: String,
}

fn main() {
    let args = Args::parse();
    if let Err(e) = run(&args) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(&args.user_agent)
        .timeout(Duration::from_secs(30))
        .danger_accept_invalid_certs(args.insecure)
        .build()?;

    // 1. Figure out the entry URL(s).
    let entries = resolve_entries(&client, &args.url, args.all_entries)?;
    if entries.is_empty() {
        return Err("no suitable JS entry file found".into());
    }
    eprintln!("Entry file(s) detected:");
    for e in &entries {
        eprintln!("  {e}");
    }
    if args.entry_only {
        for e in &entries {
            println!("{e}");
        }
        return Ok(());
    }

    // 2. For each entry, parse chunks and download.
    let host = Url::parse(&entries[0])?
        .host_str()
        .unwrap_or("dump")
        .to_string();
    let out_dir = args
        .out
        .clone()
        .unwrap_or_else(|| PathBuf::from("dump").join(&host));
    fs::create_dir_all(&out_dir)?;

    for entry in &entries {
        if is_esm(entry) {
            // Native ESM: crawl the module import graph recursively.
            eprintln!("\n{entry}: native ESM module — crawling import graph ...");
            crawl_esm(&client, entry, &out_dir, args.jobs);
            continue;
        }

        // webpack / Next.js: parse the entry, resolve chunks, download once.
        let entry_body = client.get(entry).send()?.error_for_status()?.text()?;
        let base_path = args
            .base
            .clone()
            .unwrap_or_else(|| derive_base_path(entry));
        let ext = args.ext.clone().unwrap_or_else(|| derive_extension(entry));

        let mut urls: Vec<String> = vec![entry.clone()]; // keep the entry itself
        let chunks = parse_chunks(&entry_body, entry, &base_path, &ext);
        eprintln!(
            "{}: {} chunk(s) discovered (base={base_path}, ext={ext})",
            entry,
            chunks.len()
        );
        urls.extend(chunks);

        // Dedup while preserving order.
        let mut seen = std::collections::HashSet::new();
        urls.retain(|u| seen.insert(u.clone()));

        eprintln!(
            "Downloading {} file(s) into {} ...",
            urls.len(),
            out_dir.display()
        );
        download_all(&client, &urls, &out_dir, args.jobs);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Entry detection
// ---------------------------------------------------------------------------

/// Decide whether `url` is already a JS entry, otherwise fetch the page HTML
/// and auto-detect entry script(s) from `<script src>` tags.
fn resolve_entries(
    client: &reqwest::blocking::Client,
    url: &str,
    all_entries: bool,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let path = url.split('?').next().unwrap_or("");
    let looks_like_js = ENTRY_PATTERNS
        .iter()
        .any(|p| Regex::new(p).unwrap().is_match(url))
        || path.ends_with(".js")
        || path.ends_with(".mjs");

    if looks_like_js {
        return Ok(vec![url.to_string()]);
    }

    // Fetch the page and scan for script tags.
    let html = client.get(url).send()?.error_for_status()?.text()?;
    let base = Url::parse(url)?;
    let src_re = Regex::new(r#"<script[^>]+src=["']([^"']+)["']"#).unwrap();

    // Collect every script src, resolved to an absolute URL.
    let mut srcs: Vec<String> = Vec::new();
    for cap in src_re.captures_iter(&html) {
        if let Ok(abs) = base.join(&cap[1]) {
            srcs.push(abs.to_string());
        }
    }

    // Match against patterns in priority order.
    let mut matches: Vec<String> = Vec::new();
    for pat in ENTRY_PATTERNS {
        let re = Regex::new(pat).unwrap();
        for s in &srcs {
            if re.is_match(s) && !matches.contains(s) {
                matches.push(s.clone());
            }
        }
    }

    if all_entries {
        Ok(matches)
    } else {
        Ok(matches.into_iter().take(1).collect())
    }
}

/// Base path = URL up to and including the last '/'.
fn derive_base_path(url: &str) -> String {
    match url.rfind('/') {
        Some(i) => url[..=i].to_string(),
        None => url.to_string(),
    }
}

/// Ported from popup.js `updateFileExtension`.
fn derive_extension(url: &str) -> String {
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

// ---------------------------------------------------------------------------
// Chunk parsing (mirrors content.js)
// ---------------------------------------------------------------------------

/// Returns the list of absolute chunk URLs discovered in `body`.
fn parse_chunks(body: &str, entry_url: &str, base_path: &str, ext: &str) -> Vec<String> {
    let manifest_fn =
        Regex::new(r"self\.__BUILD_MANIFEST\s*=\s*(function\s*\([^\)]*\)?\s*\{[\s\S]*?\}\s*\([^)]*\));?")
            .unwrap();
    let manifest_obj = Regex::new(r"self\.__BUILD_MANIFEST\s*=\s*(\{[\s\S]*\})").unwrap();
    let modern = Regex::new(r#"return\s+o\.p\s*\+\s*""\s*\+\s*\{([\s\S]*?)\}"#).unwrap();

    if let Some(c) = manifest_fn.captures(body) {
        return handle_manifest(&c[1], entry_url, /*is_object=*/ false);
    }
    if let Some(c) = manifest_obj.captures(body) {
        return handle_manifest(&c[1], entry_url, /*is_object=*/ true);
    }
    if let Some(c) = modern.captures(body) {
        return handle_modern(&c[1], base_path);
    }
    if entry_url.contains("webpack-runtime-") || entry_url.contains("runtime-") {
        return handle_webpack_runtime(body, base_path, ext);
    }
    handle_standard(body, base_path, ext)
}

/// Next.js build manifest: evaluate the JS expression and gather every array
/// value's string elements, then resolve each chunk against the entry URL.
fn handle_manifest(expr: &str, entry_url: &str, _is_object: bool) -> Vec<String> {
    let json = match eval_js_to_json(expr) {
        Some(v) => v,
        None => {
            eprintln!("warning: could not evaluate __BUILD_MANIFEST");
            return Vec::new();
        }
    };

    let mut chunks: Vec<String> = Vec::new();
    if let serde_json::Value::Object(map) = json {
        for (_k, v) in map {
            if let serde_json::Value::Array(items) = v {
                for it in items {
                    if let serde_json::Value::String(s) = it {
                        chunks.push(s);
                    }
                }
            }
        }
    }

    chunks
        .into_iter()
        .map(|c| find_chunk_url(entry_url, &c))
        .collect()
}

/// Evaluate a JS object/function-call expression to a JSON value using boa.
fn eval_js_to_json(expr: &str) -> Option<serde_json::Value> {
    use boa_engine::{Context, Source};
    let code = format!("JSON.stringify(({}))", expr);
    let mut ctx = Context::default();
    let res = ctx.eval(Source::from_bytes(&code)).ok()?;
    let s = res.to_string(&mut ctx).ok()?.to_std_string_escaped();
    serde_json::from_str(&s).ok()
}

/// "modern" chunks: `{id: "name", ...}` -> `<base><name>.modern.js`.
fn handle_modern(map_str: &str, base_path: &str) -> Vec<String> {
    parse_chunk_map(map_str)
        .into_values()
        .map(|name| format!("{base_path}{name}.modern.js"))
        .collect()
}

/// Standard webpack chunks: `{id:"hash"}` -> `<base><id>.<hash><ext>`.
fn handle_standard(body: &str, base_path: &str, ext: &str) -> Vec<String> {
    let map_re = Regex::new(r#"\{\s*(\d+:\s*"\w+",?\s*)+\}"#).unwrap();
    let mut out = Vec::new();
    for m in map_re.find_iter(body) {
        if let Some(map) = json_num_map(m.as_str()) {
            for (id, hash) in map {
                out.push(format!("{base_path}{id}.{hash}{ext}"));
            }
        }
    }
    out
}

/// webpack runtime: combine two `{id:"name"}` maps as `<name1>-<name2><ext>`.
fn handle_webpack_runtime(body: &str, base_path: &str, ext: &str) -> Vec<String> {
    let map_re = Regex::new(r#"\{\s*(\d+:\s*"[^"]+",?\s*)+\}"#).unwrap();
    let maps: Vec<String> = map_re.find_iter(body).map(|m| m.as_str().to_string()).collect();
    if maps.len() < 2 {
        eprintln!("warning: no chunk name mappings found in the webpack runtime");
        return Vec::new();
    }
    let m1 = parse_chunk_map(&maps[0]);
    let m2 = parse_chunk_map(&maps[1]);

    let mut keys: Vec<String> = m1.keys().chain(m2.keys()).cloned().collect();
    keys.sort();
    keys.dedup();

    keys.into_iter()
        .map(|k| {
            let n1 = m1.get(&k).cloned().unwrap_or_else(|| k.clone());
            let n2 = m2.get(&k).cloned().unwrap_or_else(|| k.clone());
            format!("{base_path}{n1}-{n2}{ext}")
        })
        .collect()
}

/// Parse `{ 0: "abc", 1: "def" }` -> ordered map, stripping quotes.
/// (Ported from content.js `parseChunkMap`.)
fn parse_chunk_map(s: &str) -> BTreeMap<String, String> {
    let trimmed = s.replace(['{', '}'], "");
    let trimmed = trimmed.trim();
    let mut map = BTreeMap::new();
    for pair in trimmed.split(',') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        if let Some((k, v)) = pair.split_once(':') {
            let k = k.trim().trim_matches('"').to_string();
            let v = v.trim().trim_matches('"').to_string();
            map.insert(k, v);
        }
    }
    map
}

/// Turn a `{0:"a",1:"b"}` literal into a string->string map via JSON,
/// mirroring content.js's `JSON.parse` after quoting numeric keys.
fn json_num_map(s: &str) -> Option<BTreeMap<String, String>> {
    let re = Regex::new(r"(\d+):").unwrap();
    let quoted = re.replace_all(s, r#""$1":"#);
    // Tolerate a trailing comma before the closing brace.
    let cleaned = Regex::new(r",\s*\}").unwrap().replace_all(&quoted, "}");
    serde_json::from_str(&cleaned).ok()
}

/// Ported from content.js `findChunkUrl`.
fn find_chunk_url(base_url: &str, chunk_name: &str) -> String {
    let base_segments: Vec<&str> = base_url.split('/').collect();
    let chunk_segments: Vec<&str> = chunk_name.split('/').collect();
    let static_index = base_segments.iter().rposition(|s| *s == "static");

    if let Some(idx) = static_index {
        let mut merged: Vec<&str> = base_segments[..=idx].to_vec();
        merged.extend_from_slice(&chunk_segments[1..]);
        merged.join("/")
    } else {
        format!("{base_url}/{chunk_name}")
    }
}

// ---------------------------------------------------------------------------
// Native ESM crawling (Framer / rolldown / rollup / Vite, ...)
// ---------------------------------------------------------------------------

/// True if the URL points at an ES module (`.mjs`), triggering graph crawling
/// instead of webpack/Next.js chunk-map parsing.
fn is_esm(url: &str) -> bool {
    url.split('?').next().unwrap_or("").ends_with(".mjs")
}

/// Breadth-first crawl of an ESM import graph, staying on the entry's host.
/// Each fetched module is saved to disk and scanned for further imports.
fn crawl_esm(
    client: &reqwest::blocking::Client,
    entry: &str,
    out_dir: &Path,
    jobs: usize,
) {
    let host = match Url::parse(entry).ok().and_then(|u| u.host_str().map(String::from)) {
        Some(h) => h,
        None => {
            eprintln!("error: invalid entry URL {entry}");
            return;
        }
    };

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(jobs.max(1))
        .build()
        .expect("thread pool");

    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut frontier: Vec<String> = vec![entry.to_string()];
    visited.insert(entry.to_string());
    let mut ok = 0usize;
    let mut failed = 0usize;

    while !frontier.is_empty() {
        // Fetch + save + extract imports for the whole frontier in parallel.
        let results: Vec<(bool, Vec<String>)> = pool.install(|| {
            frontier
                .par_iter()
                .map(|u| match fetch_and_save(client, u, out_dir) {
                    Ok(bytes) => {
                        let text = String::from_utf8_lossy(&bytes);
                        (true, resolve_esm_imports(u, &text, &host))
                    }
                    Err(e) => {
                        eprintln!("  [fail] {u}  ({e})");
                        (false, Vec::new())
                    }
                })
                .collect()
        });

        let mut next: Vec<String> = Vec::new();
        for (success, specs) in results {
            if success {
                ok += 1;
            } else {
                failed += 1;
            }
            for s in specs {
                if visited.insert(s.clone()) {
                    next.push(s);
                }
            }
        }
        if !next.is_empty() {
            eprintln!(
                "  crawled {ok} module(s), {} newly discovered ...",
                next.len()
            );
        }
        frontier = next;
    }

    eprintln!("Done: {ok} module(s) saved, {failed} failed.");
}

/// Extract every module specifier from `import`/`from`/`import(...)` statements,
/// resolve it against `base_url`, and keep same-host `.mjs`/`.js` modules.
fn resolve_esm_imports(base_url: &str, text: &str, host: &str) -> Vec<String> {
    let base = match Url::parse(base_url) {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    // Match static (`from "x"`, `import "x"`) and dynamic (`import("x")`)
    // specifiers, including backtick template literals used by Framer/rolldown
    // for lazy chunks: import(`./chunk.mjs`).
    let re = Regex::new(r#"\b(?:from|import)\s*\(?\s*["'`]([^"'`]+)["'`]"#).unwrap();

    let mut out: Vec<String> = Vec::new();
    for cap in re.captures_iter(text) {
        let spec = &cap[1];
        let path = spec.split('?').next().unwrap_or("");
        if !(path.ends_with(".mjs") || path.ends_with(".js")) {
            continue;
        }
        if let Ok(mut abs) = base.join(spec) {
            abs.set_fragment(None);
            if abs.host_str() == Some(host) {
                out.push(abs.to_string());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Fetch a URL, save it under `out_dir` preserving path structure, return bytes.
fn fetch_and_save(
    client: &reqwest::blocking::Client,
    url: &str,
    out_dir: &Path,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let bytes = client.get(url).send()?.error_for_status()?.bytes()?.to_vec();
    let path = out_dir.join(local_path_for(url));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, &bytes)?;
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// Download
// ---------------------------------------------------------------------------

fn download_all(
    client: &reqwest::blocking::Client,
    urls: &[String],
    out_dir: &Path,
    jobs: usize,
) {
    let ok = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    let errors: Mutex<Vec<String>> = Mutex::new(Vec::new());

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(jobs.max(1))
        .build()
        .expect("thread pool");

    pool.install(|| {
        urls.par_iter().for_each(|u| {
            match download_one(client, u, out_dir) {
                Ok(path) => {
                    let n = ok.fetch_add(1, Ordering::Relaxed) + 1;
                    eprintln!("  [ok {n}] {} -> {}", u, path.display());
                }
                Err(e) => {
                    failed.fetch_add(1, Ordering::Relaxed);
                    errors.lock().unwrap().push(format!("{u}  ({e})"));
                }
            }
        });
    });

    let ok = ok.load(Ordering::Relaxed);
    let failed = failed.load(Ordering::Relaxed);
    eprintln!("\nDone: {ok} downloaded, {failed} failed.");
    let errs = errors.into_inner().unwrap();
    if !errs.is_empty() {
        eprintln!("Failures:");
        for e in errs {
            eprintln!("  {e}");
        }
    }
}

fn download_one(
    client: &reqwest::blocking::Client,
    url: &str,
    out_dir: &Path,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let bytes = client.get(url).send()?.error_for_status()?.bytes()?;
    let path = out_dir.join(local_path_for(url));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, &bytes)?;
    Ok(path)
}

/// Map a chunk URL to a relative path on disk, preserving directory structure
/// (query strings are dropped).
fn local_path_for(url: &str) -> PathBuf {
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
