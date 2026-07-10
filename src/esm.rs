//! Native ESM crawling (Framer / rolldown / rollup / Vite, ...): recursively
//! follow the module import graph, bounded to the entry's host.

use std::fs;
use std::path::Path;

use rayon::prelude::*;
use regex::Regex;
use url::Url;

use crate::download::{expects_code, header_content_type, local_path_for, looks_like_html};

/// True if the URL points at an ES module (`.mjs`), triggering graph crawling
/// instead of webpack/Next.js chunk-map parsing.
pub fn is_esm(url: &str) -> bool {
    url.split('?').next().unwrap_or("").ends_with(".mjs")
}

/// Breadth-first crawl of an ESM import graph, staying on the entry's host.
/// Each fetched module is saved to disk and scanned for further imports.
pub fn crawl_esm(client: &reqwest::blocking::Client, entry: &str, out_dir: &Path, jobs: usize) {
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
            eprintln!("  crawled {ok} module(s), {} newly discovered ...", next.len());
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
/// Rejects HTML soft-404s served under a code URL so they aren't crawled/saved.
fn fetch_and_save(
    client: &reqwest::blocking::Client,
    url: &str,
    out_dir: &Path,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let resp = client.get(url).send()?.error_for_status()?;
    let content_type = header_content_type(&resp);
    let bytes = resp.bytes()?.to_vec();
    if expects_code(url) && looks_like_html(&content_type, &bytes) {
        return Err("HTML response (soft-404), skipped".into());
    }
    let path = out_dir.join(local_path_for(url));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, &bytes)?;
    Ok(bytes)
}
