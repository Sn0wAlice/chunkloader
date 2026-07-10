//! chunkloader — Rust port of the "Chunk Loader" browser extension.
//!
//! Given a page/domain URL or a direct JS entry URL (webpack runtime, Next.js
//! _buildManifest, "modern" chunk file, ...), it discovers every referenced JS
//! chunk and downloads them into a local folder for offline analysis.
//!
//! The pipeline is split across modules:
//!   - [`scan`]     entry detection + full page-asset scan + Next manifest lookup
//!   - [`chunks`]   webpack / Next.js chunk-map parsing strategies
//!   - [`esm`]      native ESM import-graph crawling (Framer / Vite / rollup)
//!   - [`flutter`]  Flutter web service-worker manifests
//!   - [`download`] parallel downloading + HTML soft-404 detection
//!
//! When no chunk map resolves, it falls back to every same-host `<script>` /
//! preloaded script on the page; referenced stylesheets are always captured, and
//! HTML soft-404s served under a `.js`/`.css` URL are detected and skipped.

mod chunks;
mod download;
mod esm;
mod flutter;
mod scan;
mod sourcemaps;

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use url::Url;

use crate::chunks::parse_chunks;
use crate::download::download_all;
use crate::esm::{crawl_esm, is_esm};
use crate::flutter::{detect_flutter, handle_flutter};
use crate::scan::{derive_base_path, derive_extension, discover_next_manifest_chunks, scan_target};

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

    /// Do not fetch `.map` source maps for downloaded JS/CSS.
    #[arg(long)]
    no_source_maps: bool,

    /// Fetch source maps but do not unpack their original sources to disk.
    #[arg(long)]
    no_extract: bool,
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

    // 0. Flutter web app? Its "chunks" are listed in flutter_service_worker.js.
    if let Some(sw_url) = detect_flutter(&client, &args.url)? {
        eprintln!("Flutter web app detected.");
        eprintln!("Service worker manifest: {sw_url}");
        if args.entry_only {
            println!("{sw_url}");
            return Ok(());
        }
        let host = Url::parse(&sw_url)?.host_str().unwrap_or("dump").to_string();
        let out_dir = out_dir_for(args, &host);
        fs::create_dir_all(&out_dir)?;
        handle_flutter(&client, &sw_url, &out_dir, args.jobs)?;
        return Ok(());
    }

    // 1. Figure out the entry URL(s) and scan the page for downloadable assets.
    let target = scan_target(&client, &args.url, args.all_entries)?;
    if target.entries.is_empty() && target.page_scripts.is_empty() {
        return Err("no suitable JS entry file found".into());
    }
    if target.entries.is_empty() {
        eprintln!("No entry pattern matched; using page <script> assets.");
    } else {
        eprintln!("Entry file(s) detected:");
        for e in &target.entries {
            eprintln!("  {e}");
        }
    }
    if args.entry_only {
        for e in &target.entries {
            println!("{e}");
        }
        return Ok(());
    }

    // 2. Resolve chunks for every entry, then layer page-level assets on top.
    let host = target
        .entries
        .first()
        .and_then(|e| Url::parse(e).ok())
        .and_then(|u| u.host_str().map(String::from))
        .or_else(|| target.base_url.as_ref().and_then(|u| u.host_str().map(String::from)))
        .unwrap_or_else(|| "dump".to_string());
    let out_dir = out_dir_for(args, &host);
    fs::create_dir_all(&out_dir)?;

    let mut urls: Vec<String> = Vec::new();
    let mut resolved_chunks = false;

    for entry in &target.entries {
        if is_esm(entry) {
            // Native ESM: crawl the module import graph recursively.
            eprintln!("\n{entry}: native ESM module — crawling import graph ...");
            crawl_esm(&client, entry, &out_dir, args.jobs);
            continue;
        }

        // webpack / Next.js: parse the entry and resolve its chunk map.
        let entry_body = client.get(entry).send()?.error_for_status()?.text()?;
        let base_path = args
            .base
            .clone()
            .unwrap_or_else(|| derive_base_path(entry));
        let ext = args.ext.clone().unwrap_or_else(|| derive_extension(entry));

        urls.push(entry.clone()); // keep the entry itself
        let chunks = parse_chunks(&entry_body, entry, &base_path, &ext);
        eprintln!(
            "{}: {} chunk(s) discovered (base={base_path}, ext={ext})",
            entry,
            chunks.len()
        );
        if !chunks.is_empty() {
            resolved_chunks = true;
        }
        urls.extend(chunks);
    }

    // Chunks resolved from a webpack runtime inlined into the page HTML
    // (CRA's default): these are the lazy chunks not present as <script> tags.
    if !target.runtime_chunks.is_empty() {
        resolved_chunks = true;
        eprintln!(
            "Inline runtime: {} chunk(s) discovered",
            target.runtime_chunks.len()
        );
        urls.extend(target.runtime_chunks.iter().cloned());
    }

    // CRA / webpack asset-manifest.json: authoritative list of every build file
    // (JS, CSS, media, fonts, maps) — catches assets no chunk map references.
    if !target.manifest_assets.is_empty() {
        eprintln!(
            "asset-manifest.json: {} file(s) listed",
            target.manifest_assets.len()
        );
        urls.extend(target.manifest_assets.iter().cloned());
    }

    // Next.js build manifest (best-effort): enumerate route chunks the
    // webpack/runtime strategies don't expose (App Router builds especially).
    if target.from_page {
        if let Some(base) = &target.base_url {
            let manifest_chunks = discover_next_manifest_chunks(&client, &target.html, base);
            if !manifest_chunks.is_empty() {
                resolved_chunks = true;
                eprintln!(
                    "Next build manifest: {} chunk(s) discovered",
                    manifest_chunks.len()
                );
                urls.extend(manifest_chunks);
            }
        }
    }

    // Same-host <script>/preloaded scripts on the page are always part of the
    // bundle: eager chunks, runtime, env-config, etc. When nothing else
    // resolved they're also the sole fallback.
    if !target.page_scripts.is_empty() {
        if resolved_chunks {
            eprintln!(
                "Including {} same-host page <script> asset(s).",
                target.page_scripts.len()
            );
        } else {
            eprintln!(
                "No chunk map resolved; using {} page <script> asset(s).",
                target.page_scripts.len()
            );
        }
        urls.extend(target.page_scripts.iter().cloned());
    }

    // Stylesheets are part of the bundle for offline auditing.
    if !target.page_styles.is_empty() {
        eprintln!("Including {} stylesheet(s).", target.page_styles.len());
        urls.extend(target.page_styles.iter().cloned());
    }

    // Dedup while preserving order.
    let mut seen = std::collections::HashSet::new();
    urls.retain(|u| seen.insert(u.clone()));

    if urls.is_empty() {
        return Ok(());
    }

    // Source maps are fetched (and unpacked) by a dedicated tolerant pass, not
    // the main download — a missing `.map` is expected, not a failure.
    let (map_urls, asset_urls): (Vec<String>, Vec<String>) = urls
        .into_iter()
        .partition(|u| u.split('?').next().unwrap_or("").ends_with(".map"));

    eprintln!(
        "Downloading {} file(s) into {} ...",
        asset_urls.len(),
        out_dir.display()
    );
    download_all(&client, &asset_urls, &out_dir, args.jobs);

    if !args.no_source_maps {
        sourcemaps::harvest(
            &client,
            &asset_urls,
            &map_urls,
            &out_dir,
            args.jobs,
            !args.no_extract,
        );
    }
    Ok(())
}

/// Output directory: `--out` override, else `dump/<host>`.
fn out_dir_for(args: &Args, host: &str) -> PathBuf {
    args.out
        .clone()
        .unwrap_or_else(|| PathBuf::from("dump").join(host))
}
