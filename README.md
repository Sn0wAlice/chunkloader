# chunkloader

Rust port of the **Chunk Loader** browser extension.

Instead of injecting JS chunks into a page, this CLI **downloads them into a local
folder** for offline analysis (reversing, auditing, source recovery of
webpack / Next.js bundles).

## What it does

Given a domain or a JS entry-file URL, it:

1. **detects the entry file** (auto-search on the page's `<script src>` tags, same
   patterns as the extension: `_buildManifest.js`, `main.*.js`, `runtime-*.js`,
   `webpack-runtime-*.js`, `app-*.js`, `*.modern.js`, …);
2. **extracts the chunk list** using several strategies:
   - Next.js `__BUILD_MANIFEST` as a function (evaluated in JS via `boa_engine`),
   - Next.js `__BUILD_MANIFEST` as an object,
   - "modern" chunks (`return o.p + "" + {…}`),
   - webpack runtime (two `{id:"name"}` maps combined into `name1-name2`),
   - standard webpack chunks (`{id:"hash"}` → `id.hash.<ext>`),
   - **native ESM** (Framer / rolldown / rollup / Vite): recursive crawl of the
     import graph (`import` / `from` / `import(\`./chunk.mjs\`)`), bounded to the same host,
   - **Flutter Web**: reads the `flutter_service_worker.js` manifest
     (`RESOURCES = {…}`) → downloads `main.dart.js`, assets, fonts, translations,
     canvaskit, etc.;
3. **downloads** everything (entry + chunks) in parallel, preserving the URL
   directory structure under `dump/<host>/`.

## Supported targets

| Type | Detection | What gets dumped |
|------|-----------|------------------|
| webpack / Next.js | entry pattern (`runtime-*.js`, `_buildManifest.js`, …) | all chunks resolved from the maps |
| native ESM (Framer, Vite…) | `.mjs` entry | full import graph (recursive) |
| Flutter Web | page that bootstraps Flutter | every resource in the service worker |

## Build

```bash
cargo build --release
# binary: ./target/release/chunkloader
```

## Usage

```bash
# From a domain (auto-detect the entry)
chunkloader https://example.com

# From a direct JS entry URL
chunkloader https://example.com/_next/static/chunks/webpack-abc123.js

# Just detect the entry without downloading
chunkloader https://example.com --entry-only

# Process ALL entries found on the page
chunkloader https://example.com --all-entries
```

### Options

| Option | Description |
|--------|-------------|
| `-o, --out <dir>` | Output directory (default: `dump/<host>`) |
| `-b, --base <path>` | Override the base path used to resolve chunks |
| `-e, --ext <ext>` | Override the chunk extension (`.chunk.js`, `.js`, …) |
| `--entry-only` | Detect and print the entry, without downloading |
| `--all-entries` | Process every detected entry, not just the best one |
| `-j, --jobs <n>` | Parallel downloads (default: 8) |
| `--insecure` | Accept invalid TLS certificates |
| `--user-agent <ua>` | Custom User-Agent |

## Note

Analysis tool: only use it against targets you are authorized to test.
