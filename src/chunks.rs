//! webpack / Next.js chunk-map parsing strategies (mirrors the extension's
//! `content.js`):
//!   1. Next.js `self.__BUILD_MANIFEST = function(...){...}(...)`  (JS eval)
//!   2. Next.js `self.__BUILD_MANIFEST = {...}`                    (JS eval)
//!   3. "modern" chunks: `return o.p + "" + {id: "name", ...}`
//!   4. webpack runtime: two `{id:"name"}` maps combined as `name1-name2`
//!   5. standard webpack chunks: `{id:"hash"}` -> `id.hash<ext>`

use std::collections::BTreeMap;

use regex::Regex;
use url::Url;

/// Returns the list of absolute chunk URLs discovered in `body`.
pub fn parse_chunks(body: &str, entry_url: &str, base_path: &str, ext: &str) -> Vec<String> {
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
    // CRA / webpack runtime chunk-filename maps, e.g.
    //   "static/js/" + (names[e]||e) + "." + hashes[e] + ".chunk.js"
    // resolved against the entry URL (which carries the origin + publicPath).
    let runtime = parse_runtime_maps(body);
    if !runtime.is_empty() {
        if let Ok(base) = Url::parse(entry_url) {
            return runtime
                .iter()
                .filter_map(|p| base.join(p).ok().map(|u| u.to_string()))
                .collect();
        }
    }
    if entry_url.contains("webpack-runtime-") || entry_url.contains("runtime-") {
        return handle_webpack_runtime(body, base_path, ext);
    }
    handle_standard(body, base_path, ext)
}

/// Parse a webpack/CRA runtime script's chunk-filename builder(s) into chunk
/// paths (prefixed with `publicPath`, ready to resolve against the page/entry).
///
/// Handles both the plain form `"<prefix>" + e + "." + {hashes}[e] + "<suffix>"`
/// and the named form `"<prefix>" + ({names}[e] || e) + "." + {hashes}[e] +
/// "<suffix>"`, for JS (`static/js/….chunk.js`) and CSS (`static/css/….chunk.css`)
/// alike. Returns an empty vec when the script has no such builder.
pub fn parse_runtime_maps(script: &str) -> Vec<String> {
    let public_path = Regex::new(r#"\.p\s*=\s*"([^"]*)""#)
        .unwrap()
        .captures(script)
        .map(|c| c[1].to_string())
        .filter(|p| p != "auto")
        .unwrap_or_else(|| "/".to_string());

    // Prepend publicPath unless the prefix is already absolute (leading '/' or a
    // full URL), to avoid producing a protocol-relative `//host` path.
    let build = |prefix: &str, name: &str, hash: &str, suffix: &str| -> String {
        let tail = format!("{name}.{hash}{suffix}");
        if prefix.starts_with('/') || prefix.starts_with("http") {
            format!("{prefix}{tail}")
        } else {
            format!("{public_path}{prefix}{tail}")
        }
    };
    let wanted = |prefix: &str, suffix: &str| {
        prefix.contains('/') && (suffix.ends_with(".js") || suffix.ends_with(".css"))
    };

    let mut out: Vec<String> = Vec::new();

    // Named form: "<prefix>" + ({names}[e] || e) + "." + {hashes}[e] + "<suffix>"
    let named = Regex::new(
        r#""([^"]*)"\s*\+\s*\(\s*(\{[^{}]*\})\s*\[\s*\w+\s*\]\s*\|\|\s*\w+\s*\)\s*\+\s*"\."\s*\+\s*(\{[^{}]*\})\s*\[\s*\w+\s*\]\s*\+\s*"([^"]*)""#,
    )
    .unwrap();
    for c in named.captures_iter(script) {
        let (prefix, suffix) = (&c[1], &c[4]);
        if !wanted(prefix, suffix) {
            continue;
        }
        let names = json_num_map(&c[2]).unwrap_or_default();
        let hashes = json_num_map(&c[3]).unwrap_or_default();
        for (id, hash) in &hashes {
            let name = names.get(id).cloned().unwrap_or_else(|| id.clone());
            out.push(build(prefix, &name, hash, suffix));
        }
    }

    // Plain form: "<prefix>" + e + "." + {hashes}[e] + "<suffix>"
    let plain = Regex::new(
        r#""([^"]*)"\s*\+\s*\w+\s*\+\s*"\."\s*\+\s*(\{[^{}]*\})\s*\[\s*\w+\s*\]\s*\+\s*"([^"]*)""#,
    )
    .unwrap();
    for c in plain.captures_iter(script) {
        let (prefix, suffix) = (&c[1], &c[3]);
        if !wanted(prefix, suffix) {
            continue;
        }
        let hashes = match json_num_map(&c[2]) {
            Some(m) => m,
            None => continue,
        };
        for (id, hash) in &hashes {
            out.push(build(prefix, id, hash, suffix));
        }
    }

    out.sort();
    out.dedup();
    out
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
