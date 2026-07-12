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
mod ui;

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
use crate::ui::{Card, Line};

/// Discovery tallies gathered while resolving a target, rendered into the
/// final summary card.
#[derive(Default)]
struct Discovery {
    chunks: usize,
    runtime: usize,
    manifest: usize,
    next_manifest: usize,
    scripts: usize,
    styles: usize,
}

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
        if args.entry_only {
            println!("{sw_url}");
            return Ok(());
        }
        let host = Url::parse(&sw_url)?.host_str().unwrap_or("dump").to_string();
        let out_dir = out_dir_for(args, &host);
        fs::create_dir_all(&out_dir)?;
        let stats = handle_flutter(&client, &sw_url, &out_dir, args.jobs)?;
        render_card(&host, "Flutter web app", &Discovery::default(), &stats, None, &out_dir);
        return Ok(());
    }

    // 1. Figure out the entry URL(s) and scan the page for downloadable assets.
    eprintln!("{} {}", ui::dim("▸ scanning"), ui::bold(&args.url));
    let target = scan_target(&client, &args.url, args.all_entries)?;
    if target.entries.is_empty() && target.page_scripts.is_empty() {
        return Err("no suitable JS entry file found".into());
    }
    if args.entry_only {
        for e in &target.entries {
            println!("{e}");
        }
        return Ok(());
    }
    let mut disco = Discovery::default();

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
    let mut esm_stats = download::DownloadStats::default();

    for entry in &target.entries {
        if is_esm(entry) {
            // Native ESM: crawl the module import graph recursively.
            let (ok, failed) = crawl_esm(&client, entry, &out_dir, args.jobs);
            esm_stats.ok += ok;
            esm_stats.failed += failed;
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
        disco.chunks += chunks.len();
        urls.extend(chunks);
    }

    // Chunks resolved from a webpack runtime inlined into the page HTML
    // (CRA's default): these are the lazy chunks not present as <script> tags.
    if !target.runtime_chunks.is_empty() {
        disco.runtime = target.runtime_chunks.len();
        urls.extend(target.runtime_chunks.iter().cloned());
    }

    // CRA / webpack asset-manifest.json: authoritative list of every build file
    // (JS, CSS, media, fonts, maps) — catches assets no chunk map references.
    if !target.manifest_assets.is_empty() {
        disco.manifest = target.manifest_assets.len();
        urls.extend(target.manifest_assets.iter().cloned());
    }

    // Next.js build manifest (best-effort): enumerate route chunks the
    // webpack/runtime strategies don't expose (App Router builds especially).
    if target.from_page {
        if let Some(base) = &target.base_url {
            let manifest_chunks = discover_next_manifest_chunks(&client, &target.html, base);
            disco.next_manifest = manifest_chunks.len();
            urls.extend(manifest_chunks);
        }
    }

    // Same-host <script>/preloaded scripts on the page are always part of the
    // bundle: eager chunks, runtime, env-config, etc. When nothing else
    // resolved they're also the sole fallback.
    disco.scripts = target.page_scripts.len();
    urls.extend(target.page_scripts.iter().cloned());

    // Stylesheets are part of the bundle for offline auditing.
    disco.styles = target.page_styles.len();
    urls.extend(target.page_styles.iter().cloned());

    // Dedup while preserving order.
    let mut seen = std::collections::HashSet::new();
    urls.retain(|u| seen.insert(u.clone()));

    // Source maps are fetched (and unpacked) by a dedicated tolerant pass, not
    // the main download — a missing `.map` is expected, not a failure.
    let (map_urls, asset_urls): (Vec<String>, Vec<String>) = urls
        .into_iter()
        .partition(|u| u.split('?').next().unwrap_or("").ends_with(".map"));

    let mut stats = download_all(&client, &asset_urls, &out_dir, args.jobs);
    // Fold ESM crawl results into the overall download tally.
    stats.ok += esm_stats.ok;
    stats.failed += esm_stats.failed;

    let smaps = if !args.no_source_maps {
        Some(sourcemaps::harvest(
            &client,
            &asset_urls,
            &map_urls,
            &out_dir,
            args.jobs,
            !args.no_extract,
        ))
    } else {
        None
    };

    // Subtitle: the detected entry file (or the page-assets fallback).
    let subtitle = match target.entries.first() {
        Some(first) => {
            let name = first.split('?').next().unwrap_or(first).rsplit('/').next().unwrap_or(first);
            if target.entries.len() > 1 {
                format!("{name}  +{} more", target.entries.len() - 1)
            } else {
                name.to_string()
            }
        }
        None => "page <script> assets".to_string(),
    };

    render_card(&host, &subtitle, &disco, &stats, smaps.as_ref(), &out_dir);
    Ok(())
}

/// Render the final coloured summary card to stderr.
fn render_card(
    host: &str,
    subtitle: &str,
    disco: &Discovery,
    stats: &download::DownloadStats,
    smaps: Option<&sourcemaps::SourceMapStats>,
    out_dir: &PathBuf,
) {
    let chunks_total = disco.chunks + disco.runtime + disco.next_manifest;
    let mut card = Card::new(&format!("chunkloader · {host}"), ui::BRAND);

    // The detected entry / target, then a blank spacer.
    card = card
        .line(Line::new().styled("2", subtitle))
        .blank();

    // Discovery breakdown — only show rows that carry a signal.
    let mut disco_rows: Vec<Line> = Vec::new();
    let mut row = Line::new();
    let mut cells = 0;
    let push = |row: &mut Line, cells: &mut usize, rows: &mut Vec<Line>, label: &str, n: usize, code: &str| {
        if n == 0 {
            return;
        }
        *row = std::mem::take(row).stat(label, n, code);
        *cells += 1;
        if *cells == 2 {
            rows.push(std::mem::take(row));
            *cells = 0;
        }
    };
    push(&mut row, &mut cells, &mut disco_rows, "chunks", chunks_total, ui::CHUNK);
    push(&mut row, &mut cells, &mut disco_rows, "assets", disco.manifest, ui::ASSET);
    push(&mut row, &mut cells, &mut disco_rows, "scripts", disco.scripts, ui::SCRIPT);
    push(&mut row, &mut cells, &mut disco_rows, "styles", disco.styles, ui::STYLE);
    if let Some(s) = smaps {
        push(&mut row, &mut cells, &mut disco_rows, "maps", s.fetched, ui::SOURCE);
        push(&mut row, &mut cells, &mut disco_rows, "sources", s.sources, ui::SOURCE);
    }
    if cells > 0 {
        disco_rows.push(row);
    }
    for r in disco_rows {
        card = card.line(r);
    }

    // Result line with status glyphs.
    card = card.blank().line(
        Line::new()
            .styled(ui::OK, &format!("✓ {} downloaded", stats.ok))
            .plain("   ")
            .styled(
                if stats.skipped > 0 { ui::WARN } else { "2" },
                &format!("⚠ {} skipped", stats.skipped),
            )
            .plain("   ")
            .styled(
                if stats.failed > 0 { ui::FAIL } else { "2" },
                &format!("✗ {} failed", stats.failed),
            ),
    );

    card = card
        .blank()
        .line(Line::new().styled("2", "→ ").styled("2", &out_dir.display().to_string()));

    card.print();

    // Detailed failures below the card, if any.
    if !stats.errors.is_empty() {
        eprintln!("{}", ui::paint(ui::FAIL, "failures:"));
        for e in &stats.errors {
            eprintln!("  {}", ui::dim(e));
        }
    }
}

/// Output directory: `--out` override, else `dump/<host>`.
fn out_dir_for(args: &Args, host: &str) -> PathBuf {
    args.out
        .clone()
        .unwrap_or_else(|| PathBuf::from("dump").join(host))
}
