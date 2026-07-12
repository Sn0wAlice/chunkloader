//! Source-map harvesting: fetch `.js.map` / `.css.map` siblings of downloaded
//! assets and unpack their embedded `sourcesContent` into the original source
//! tree — this is what recovers the actual (pre-bundle) source code.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use rayon::prelude::*;

use crate::download::local_path_for;

/// For every JS/CSS asset (and every explicit `.map` URL), fetch the source map
/// and, when `extract` is set, write each original source under `<out>/_sources/`.
/// Number of `.map` files fetched and original source files unpacked.
#[derive(Default)]
pub struct SourceMapStats {
    pub fetched: usize,
    pub sources: usize,
}

pub fn harvest(
    client: &reqwest::blocking::Client,
    assets: &[String],
    explicit_maps: &[String],
    out_dir: &Path,
    jobs: usize,
    extract: bool,
) -> SourceMapStats {
    // Candidate map URLs: the ones we already know about, plus a `.map` sibling
    // for each downloaded JS/CSS asset.
    let mut maps: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |u: String, maps: &mut Vec<String>| {
        if seen.insert(u.clone()) {
            maps.push(u);
        }
    };
    for m in explicit_maps {
        push(m.clone(), &mut maps);
    }
    for a in assets {
        let path = a.split('?').next().unwrap_or("");
        if path.ends_with(".map") {
            push(a.clone(), &mut maps);
        } else if path.ends_with(".js") || path.ends_with(".mjs") || path.ends_with(".css") {
            push(format!("{a}.map"), &mut maps);
        }
    }
    if maps.is_empty() {
        return SourceMapStats::default();
    }

    let fetched = AtomicUsize::new(0);
    let sources = AtomicUsize::new(0);

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(jobs.max(1))
        .build()
        .expect("thread pool");

    pool.install(|| {
        maps.par_iter().for_each(|m| {
            let bytes = match client.get(m).send().and_then(|r| r.error_for_status()) {
                Ok(resp) => match resp.bytes() {
                    Ok(b) => b,
                    Err(_) => return,
                },
                Err(_) => return, // 404 / not deployed — silent skip
            };
            // Source maps are JSON objects; ignore anything else (e.g. soft-404).
            let is_json_object = bytes
                .iter()
                .find(|b| !b.is_ascii_whitespace())
                .is_some_and(|b| *b == b'{');
            if !is_json_object {
                return;
            }
            // Save the raw .map alongside its asset.
            let path = out_dir.join(local_path_for(m));
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::write(&path, &bytes);
            fetched.fetch_add(1, Ordering::Relaxed);

            if extract {
                let n = extract_sources(&bytes, out_dir);
                sources.fetch_add(n, Ordering::Relaxed);
            }
        });
    });

    SourceMapStats {
        fetched: fetched.load(Ordering::Relaxed),
        sources: sources.load(Ordering::Relaxed),
    }
}

/// Unpack a source map's `sourcesContent` into `<out>/_sources/<path>`.
/// Returns the number of source files written.
fn extract_sources(map_bytes: &[u8], out_dir: &Path) -> usize {
    let v: serde_json::Value = match serde_json::from_slice(map_bytes) {
        Ok(v) => v,
        Err(_) => return 0,
    };
    let sources = match v.get("sources").and_then(|s| s.as_array()) {
        Some(s) => s,
        None => return 0,
    };
    let contents = v.get("sourcesContent").and_then(|s| s.as_array());
    let root = out_dir.join("_sources");

    let mut written = 0usize;
    for (i, src) in sources.iter().enumerate() {
        let Some(src) = src.as_str() else { continue };
        // Only write sources that carry inline content.
        let content = match contents.and_then(|c| c.get(i)).and_then(|c| c.as_str()) {
            Some(c) => c,
            None => continue,
        };
        let rel = sanitize_source_path(src);
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            if fs::create_dir_all(parent).is_err() {
                continue;
            }
        }
        if fs::write(&path, content).is_ok() {
            written += 1;
        }
    }
    written
}

/// Map a source-map `sources` entry (e.g. `webpack://app/./src/A.js`,
/// `../node_modules/x/index.js`) to a safe relative path, dropping any scheme
/// and `..`/`.` components so it can't escape `_sources/`.
fn sanitize_source_path(src: &str) -> PathBuf {
    let mut s = src.split(['?', '#']).next().unwrap_or(src);
    if let Some(idx) = s.find("://") {
        s = &s[idx + 3..]; // strip scheme like webpack://
    }
    let s = s.trim_start_matches('/');
    let rel: PathBuf = Path::new(s)
        .components()
        .filter(|c| matches!(c, std::path::Component::Normal(_)))
        .collect();
    if rel.as_os_str().is_empty() {
        PathBuf::from("unknown_source")
    } else {
        rel
    }
}
