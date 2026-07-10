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
   - **Next.js App Router build manifest** (`/_next/static/<buildId>/_buildManifest.js`,
     best-effort when a build id is present on the page),
   - **Flutter Web**: reads the `flutter_service_worker.js` manifest
     (`RESOURCES = {…}`) → downloads `main.dart.js`, assets, fonts, translations,
     canvaskit, etc.;
3. **falls back** to every same-host `<script>` / preloaded script referenced by
   the page when no chunk map resolves (e.g. minimal App Router builds), and
   always captures referenced **stylesheets** (`<link rel="stylesheet">` plus
   `.css` paths embedded in the inline RSC/flight payload);
4. **downloads** everything (entry + chunks + assets) in parallel, preserving the
   URL directory structure under `dump/<host>/`. HTML soft-404s served under a
   `.js`/`.css` URL are detected and skipped instead of being saved as code.

## Supported targets

| Type | Detection | What gets dumped |
|------|-----------|------------------|
| webpack / Next.js | entry pattern (`runtime-*.js`, `_buildManifest.js`, …) | all chunks resolved from the maps |
| native ESM (Framer, Vite…) | `.mjs` entry | full import graph (recursive) |
| Flutter Web | page that bootstraps Flutter | every resource in the service worker |
| any page (fallback) | no chunk map resolves | every same-host `<script>` + stylesheet on the page |

## Installation

### Homebrew (macOS / Linux)

```bash
brew tap Sn0wAlice/chunkloader https://github.com/Sn0wAlice/chunkloader
brew install chunkloader
```

### Pre-built binary

Grab the latest `.tar.gz` (or `.deb` on Debian/Ubuntu) for your platform from the
[Releases](https://github.com/Sn0wAlice/chunkloader/releases/latest) page:

```bash
tar xzf chunkloader-linux-amd64.tar.gz
sudo install -m755 chunkloader /usr/local/bin/
```

On Debian/Ubuntu:

```bash
sudo dpkg -i chunkloader_*_amd64.deb
```

### From source

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
