# Hoard architecture and software stack

Hoard is one Rust binary per node. It does three jobs over one shared index:

1. **Index**: discover model, dataset and repository files across nominated disks, sniff
   headers, infer role and base from tensors, deduplicate by content hash, answer the search
   grammar with facets and directional compatibility, and keep a triage queue for what it
   cannot place.
2. **Serve** three faces on the LAN: the HuggingFace Hub HTTP API (`HF_ENDPOINT`), git over
   HTTP with LFS, and a symlink projection into tool layouts. Repos not held locally are
   proxied from `huggingface.co` and block-cached, following the contract of the Python
   reference `olah`.
3. **Federate**: nodes discover each other, gossip index digests, and pull from each other
   through the same Hub face, so a peer is just another upstream.

The filesystem stays authoritative. Hoard never moves, renames or rewrites a discovered file.
All meaning (repo, revision, artifact, role, node) lives in the index.

## Runtime stack

Toolchain: Rust 1.95 stable, edition 2024, workspace resolver 3.

| Concern | Crate | Version | Why |
| --- | --- | --- | --- |
| Async runtime | `tokio` | 1.53 | Required by AGENTS.md; multi-thread runtime, `fs`, `signal`, `sync`, `process` for upload-pack. |
| HTTP server | `axum` | 0.8 | Required by AGENTS.md; handlers return `Result<Response, AppError>`. |
| Middleware | `tower`, `tower-http` | 0.5, 0.7 | Trace, timeout, request-id, compression (text only), CORS, catch-panic, body limit. |
| Typed headers, Range | `axum-extra`, `headers`, `http-range-header` | 0.12, 0.4, 0.4 | `Range` and `ETag` on `resolve` without hand parsing. |
| Upstream and peer client | `reqwest` | 0.13 | rustls, streaming bodies, HTTP/2 to `huggingface.co`, the LFS CDN and peer nodes. |
| Static UI embedding | `rust-embed` | 8.12 | Ships `web/` inside the binary in release, reads from disk in debug. |
| Serialization | `serde`, `serde_json`, `serde_yaml_ng`, `toml` | 1.0, 1.0, 0.10, 1.1 | JSON API, README frontmatter, `hoard.toml`, loadout files. |
| CLI and config | `clap` (derive, env), `dotenvy`, `secrecy` | 4.6, 0.15, 0.10 | Subcommands; tokens only via env or `.env`. |
| Errors | `thiserror`, `anyhow` | 2.0, 1.0 | Typed errors per crate, `anyhow` only in the binary. |
| Logging | `tracing`, `tracing-subscriber` | 0.1, 0.3 | Structured logs, `RUST_LOG` filtering. |
| Progress | `indicatif` | 0.18 | Scan, hash and pull progress in the CLI. |
| Index store | `rusqlite` (bundled), `r2d2_sqlite`, `rusqlite_migration` | 0.40, 0.35, 2.6 | SQLite WAL with FTS5; one writer, pooled readers, versioned migrations. |
| Native catalog (experimental) | `native_db`, `native_model`, `redb`, `tantivy` | 0.8.2, =0.4.20, 2.6, 0.26 | Typed records with automatic secondary indexes on a single redb file; Tantivy is a rebuildable search index. Only the `hoard catalog` commands use it. |
| Discovery walk | `ignore`, `globset`, `notify` | 0.4, 0.4, 8.2 | Parallel walker with `.hoardignore`, include/exclude globs, live re-index. |
| Parallel CPU work | `rayon` | 1.12 | Hashing, header sniffing, inference. |
| Artifact formats | none (hand-written header readers) | | safetensors and GGUF headers are read directly; see "Not chosen and why". |
| Hashing | `sha2`, `sha1`, `xxhash-rust` (xxh3) | 0.11, 0.11, 0.8 | sha256 for LFS oid and ETag parity, git blob sha1, xxh3 fingerprints. |
| Local git repos | `gix` (`sha1`, `revision`, `parallel`) | 0.87 | Refs, trees, blobs, commits of clones; upload-pack itself is delegated to the `git` binary. `parallel` is what makes a repository handle `Send`. |
| Search grammar | `winnow` | 1.0 | Tokenizer and parser for the query grammar and the token strip. |
| README rendering | `pulldown-cmark` | 0.13 | Server-side Markdown to HTML. |
| In-memory caches | `moka` (future), `arc-swap`, `parking_lot` | 0.12, 1.9, 0.12 | Upstream metadata TTL, hot-swappable config and rules. |
| Time | `jiff` | 0.2 | Timestamps, `lastModified`, `used:>30d` arithmetic. |
| Enum spellings | `strum` | 0.28 | `Role`, `Base`, `State` serialize to their grammar spelling. |
| Sizes | `humansize` | 2.1 | `228 MB`, `26.4 GB` in mono columns. |
| LAN discovery | `mdns-sd` | 0.16 | `_hoard._tcp` advertisement and browsing in `hoard-net`. |
| CRC | `crc32fast` | 1.5 | The per-chunk CRC32 of the olah block-cache format. |
| Tests | `tempfile`, `wiremock`, `insta`, `mockall`, `criterion` | 3.27, 0.6, 1.48, 0.15, 0.8 | Temp trees, mocked upstream and peers, snapshots of API JSON, benches. |

Not chosen and why:

- `sqlx`: async SQLite goes through a thread pool anyway; `rusqlite` behind `spawn_blocking`
  is smaller and gives FTS5 and `user_version` migrations directly.
- `git2`: links libgit2; `gix` is pure Rust and read paths are all the index needs.
- `tantivy` alone: it is a search index, not a store. It is used beside `native_db` in the
  experimental catalog; see `docs/superpowers/specs/2026-09-05-native-catalog-decision.md`
  for why that pairing was chosen over SQLite FTS5 and over Fjall.
- `gguf-rs`: pulls `clap`, `comfy-table` and a logger into a library.
- `memmap2`: every mapping constructor is `unsafe`, and the workspace sets
  `unsafe_code = "forbid"`. Headers are read with bounded `File` reads instead, which costs
  nothing here because a sniffer stops after a few kilobytes anyway.
- `safetensors`: `read_metadata` wants the whole file in a buffer and rejects non-string
  `__metadata__` values, which are exactly the kohya `ss_tag_frequency` objects hoard reads.
- `ggus`: GGUF v3 only, native endian only, `unwrap`s internally, slices the whole tensor
  payload (so a truncated file panics), and spells types `Q4K` rather than `Q4_K`. The
  hand-written parser is about 200 lines, reads v2 and v3, and never allocates on a corrupt
  length.
- `dioxus`/WASM: the UI is dense tables and a query bar; vanilla JS over a JSON API loads
  faster and matches AGENTS.md. Parsing, Markdown, facets and compat run on the server.

## Frontend stack

- Pico CSS 2.1.1 vendored at `web/vendor/pico.min.css`.
- `web/css/tokens.css` holds every color, font and dimension from `docs/design/ui-spec.md`,
  light-first with dark overrides; `web/css/hoard.css` styles components with those tokens.
- Space Grotesk and Space Mono vendored as woff2 under `web/fonts/` with local `@font-face`.
- Vanilla ES modules in `web/js/`, no bundler. JS renders JSON from `/api/v1/*` and handles
  keyboard and mouse input; nothing that could be unit tested lives in JS.

## Crate layout

```
hoard/
  Cargo.toml                 workspace root and the `hoard` binary
  CLAUDE.md  AGENTS.md       agent instructions (CLAUDE.md imports AGENTS.md)
  .claude/                   settings, hooks, rules, skills, agents for Claude Code
  clippy.toml  rustfmt.toml
  hoard.example.toml         config template (copy to hoard.toml, git-ignored)
  docs/
    use-case.md              designer's brief
    feature-plan.md          scope, phases, acceptance, status table
    architecture.md          this file
    api/hub-compat.md        HuggingFace Hub routes, derived from olah
    api/hoard-api.md         /api/v1 JSON contract for UI and CLI
    design/ui-spec.md        tokens, layout, columns, keys extracted from the design
    design/handoff/          Claude Design export, verbatim
  src/
    main.rs                  clap entry
    cli/                     serve, scan, ls, path, loadout, apply, link, node, doctor
  crates/
    hoard-core/              domain types, no IO
      src/
        id.rs                RepoId, RepoType, CommitSha, Sha256, BlobId, ArtifactId, NodeId
        artifact.rs          Artifact, ArtifactFormat, Precision, TensorSummary
        role.rs              Role, SubRole, Base with grammar aliases
        state.rs             State (served, local, pulling, stale, remote, triage)
        evidence.rs          Evidence { field, value, source, detail, confidence }
        compat.rs            directional compatibility rule
        loadout.rs           Loadout document, TOML (de)serialization
        error.rs             HoardError
    hoard-index/             discovery, sniffing, inference, storage, search
      src/
        config.rs            IndexConfig: sources, globs, hashing policy, layouts
        discover/
          walker.rs          ignore-based parallel walk
          hf_cache.rs        ~/.cache/huggingface/hub layout
          git_clone.rs       clones with LFS pointers
          loose.rs           bare folders and known tool layouts
        sniff/
          safetensors.rs     header and __metadata__ via memmap
          gguf.rs            metadata via ggus
          pickle.rs          zip listing for .ckpt/.pt/.pth/.bin
          readme.rs          frontmatter and body
          dataset.rs         parquet, arrow, jsonl, csv
        infer/
          rules.rs           key prefix and shape signature tables
          role.rs  base.rs  lora.rs  precision.rs  trigger.rs
        triage.rs            queue, confirm, reject, noise list
        hash/                fingerprint, sha256, git blob sha1
        store/
          schema.sql  migrations.rs  pool.rs  convert.rs  fixture.rs
          node.rs  source.rs  blob.rs  artifact.rs  repo.rs  revision.rs  entry.rs
          shelf.rs  loadout.rs  job.rs  usage.rs  overrides.rs  setting.rs
          projection.rs  search_exec.rs  row.rs  digest.rs
        catalog/             experimental native_db + Tantivy catalog behind `hoard catalog`
          mod.rs             Catalog open/status/integrity, format version
          models.rs          native_model records: Source, Repository, ArtifactRecord, Entry, SearchEntry, SearchText, Manifest
          scan.rs            discovery -> sniff -> infer -> one transaction per source
          search.rs          Tantivy schema, rebuild, per-entry documents collapsed per artifact
        search/
          grammar.rs         winnow parser
          token.rs           token kinds and the parsed strip
          sql.rs             Query -> WHERE and ORDER
          facets.rs          per-dimension counts excluding own key, disk pressure
        shard.rs             groups a model split across files into one artifact
        scan.rs              discover -> sniff -> hash -> infer -> store
        watch.rs             notify-driven incremental rescans
    hoard-mirror/            serving faces and upstreams, transport-agnostic
      src/
        config.rs            MirrorConfig: upstreams, cache root, rules, offline, xet
        access.rs            proxy and cache rules
        model/               Hub JSON shapes
        local/
          index_repo.rs      repo metadata and bytes from the index
          git_repo.rs        gix-backed trees, blobs, LFS objects
        upstream/
          client.rs          reqwest client, header forwarding, redirects
          hub.rs             huggingface.co metadata and files
          peer.rs            another hoard node as an upstream
          range.rs           Range header parsing and clamping, 416 decisions
          meta.rs            metadata request kinds, their URLs and cache paths
        cache/
          layout.rs  entry.rs  block.rs  policy.rs  meta.rs
        git/
          smart_http.rs      info/refs and upload-pack via the git binary
          lfs_batch.rs       LFS batch API pointing at resolve
        resolve.rs           revision -> sha, local first
        ingest.rs            completed cache entries into the index
        usage.rs             served-request log
    hoard-net/               node identity, mDNS discovery, digest gossip, peer pulls
      src/
        identity.rs          NodeId, name, advertised URL and the _hoard._tcp TXT record
        discovery.rs         mDNS advertise and browse, with the static peer list as peer
        digest.rs            the compact index digest, its cursors and how one is applied
        peer.rs              the peer client, pulls through the Hub face, state derivation
        error.rs             NetError
    hoard-server/            axum wiring only
      src/
        state.rs  error.rs  layers.rs  assets.rs
        hub/                 Hub-compatible routes
          mod.rs  meta.rs  files.rs  proxy.rs
        git/                 git smart HTTP and LFS routes (mod.rs)
        api/                 /api/v1: search, compat, artifacts, repos, readme, shelves,
                             loadouts, triage, sources, jobs, nodes, usage, link, apply, stats
        ui/                  embedded web/ and index.html fallback
  web/
    index.html
    css/tokens.css  css/hoard.css  vendor/pico.min.css  fonts/
    js/
      app.js  api.js  search.js  facets.js  table.js  cards.js  inspector.js
      detail.js  readme.js  loadout.js  shelves.js  nodes.js  jobs.js  keys.js  theme.js
  tests/                     end-to-end: temp tree, router, Hub and git clients
  benches/                   walker, grammar, facets
```

Dependency direction is strict: `hoard-core` <- `hoard-index` <- `hoard-mirror` <-
`hoard-net` <- `hoard-server` <- `hoard`. No crate below `hoard-server` knows about axum.

## Data flow

Scan:

```
sources -> discover -> sniff (headers only) -> fingerprint -> infer (role, base, rank,
precision, trigger, confidence) -> store; sha256 in a background job; low confidence -> triage
```

Serve, local hit on the Hub face:

```
GET /org/name/resolve/main/file.safetensors
  -> resolve revision (index) -> entry -> blob path
  -> stream with Range, x-repo-commit, etag = "sha256", x-linked-size -> usage log
```

Serve, miss (peer or huggingface.co, rules permitting):

```
  -> choose upstream (peer holding the sha256 first, then hub)
  -> metadata (moka TTL) -> block cache: serve present blocks, fetch missing with Range
  -> persist blocks and CRCs -> on completion verify sha256 and ingest into the index
```

Git face:

```
GET /org/repo/info/refs?service=git-upload-pack -> spawn git upload-pack --stateless-rpc
POST /org/repo.git/info/lfs/objects/batch -> hrefs to /org/repo/resolve/<sha>/<path>
```

Nodes:

```
mDNS _hoard._tcp -> peers table -> periodic GET /api/v1/peers/digest?since=cursor
  -> REMOTE entries in the local index -> `hoard apply` pulls via the proxy path
  -> hourly full re-read retires the rows a peer stopped naming
```

## Storage

- Index: `<data-dir>/hoard.db` (SQLite WAL), recreatable from disk plus peers.
- Native catalog: any directory given to `hoard catalog scan --directory`, holding
  `catalog.redb` plus `search-v1-<nanos>/` Tantivy directories. Not written by `serve`.
- Mirror cache: `<data-dir>/repos/` in the olah layout (`docs/api/hub-compat.md`).
- Projections: symlinks written by `hoard link` and `hoard apply`, tracked in the index so
  PATHS ON DISK can show `laptop · symlink`.
- Config: `hoard.toml`; secrets only in `.env` (`HF_TOKEN`, `HOARD_NODE_TOKEN`).
