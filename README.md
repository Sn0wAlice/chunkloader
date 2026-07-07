# chunkloader

Portage en Rust de l'extension navigateur **Chunk Loader**.

Au lieu d'injecter les chunks JS dans une page, ce CLI les **télécharge dans un
dossier local** pour analyse hors-ligne (reverse, audit, source recovery de bundles
webpack / Next.js).

## Ce que ça fait

À partir d'un domaine ou d'une URL de fichier JS d'entrée, il :

1. **détecte le fichier d'entrée** (auto-search sur les `<script src>` de la page,
   mêmes patterns que l'extension : `_buildManifest.js`, `main.*.js`, `runtime-*.js`,
   `webpack-runtime-*.js`, `app-*.js`, `*.modern.js`, …) ;
2. **extrait la liste des chunks** selon 5 stratégies reprises de `content.js` :
   - Next.js `__BUILD_MANIFEST` sous forme de fonction (évaluée en JS via `boa_engine`),
   - Next.js `__BUILD_MANIFEST` sous forme d'objet,
   - chunks « modern » (`return o.p + "" + {…}`),
   - runtime webpack (deux maps `{id:"name"}` combinées en `name1-name2`),
   - chunks webpack standard (`{id:"hash"}` → `id.hash.<ext>`) ;
3. **télécharge** tout (entrée + chunks) en parallèle, en conservant l'arborescence
   des URLs sous `dump/<host>/`.

## Build

```bash
cargo build --release
# binaire : ./target/release/chunkloader
```

## Usage

```bash
# À partir d'un domaine (auto-détection de l'entrée)
chunkloader https://example.com

# À partir d'une URL de fichier JS directe
chunkloader https://example.com/_next/static/chunks/webpack-abc123.js

# Juste détecter l'entrée sans télécharger
chunkloader https://example.com --entry-only

# Traiter TOUTES les entrées trouvées sur la page
chunkloader https://example.com --all-entries
```

### Options

| Option | Description |
|--------|-------------|
| `-o, --out <dir>` | Dossier de sortie (défaut : `dump/<host>`) |
| `-b, --base <path>` | Force le base path de résolution des chunks |
| `-e, --ext <ext>` | Force l'extension des chunks (`.chunk.js`, `.js`, …) |
| `--entry-only` | Détecte et affiche l'entrée, sans rien télécharger |
| `--all-entries` | Traite chaque entrée détectée, pas seulement la meilleure |
| `-j, --jobs <n>` | Téléchargements parallèles (défaut : 8) |
| `--insecure` | Accepte les certificats TLS invalides |
| `--user-agent <ua>` | User-Agent personnalisé |

## Note

Outil d'analyse : à n'utiliser que sur des cibles que vous êtes autorisé à tester.
