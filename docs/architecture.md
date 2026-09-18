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
| HTTP server | `axum` | 0.8 | Required by AGENTS.md; handlers return `ApiResult<T>`, the crate's `Result<T, ApiError>`. |
| Middleware | `tower`, `tower-http` | 0.5, 0.7 | Trace, timeout, request-id, compression (text only), CORS, catch-panic, body limit. |
| Typed headers, Range | `axum-extra`, `headers`, `http-range-header` | 0.12, 0.4, 0.4 | `Range` and `ETag` on `resolve` without hand parsing. |
| Upstream and peer client | `reqwest` | 0.13 | rustls, streaming bodies, HTTP/2 to `huggingface.co`, the LFS CDN and peer nodes. |
| Static UI embedding | `rust-embed` | 8.12 | Ships `web/` inside the binary in release, reads from disk in debug. |
| Serialization | `serde`, `serde_json`, `serde_yaml_ng`, `toml` | 1.0, 1.0, 0.10, 1.1 | JSON API, README frontmatter, `hoard.toml`, loadout files. |
| CLI and config | `clap` (derive, env), `dotenvy`, `secrecy` | 4.6, 0.15, 0.10 | Subcommands; tokens only via env or `.env`. |
| Errors | `thiserror`, `anyhow` | 2.0, 1.0 | Typed errors per crate, `anyhow` only in the binary. |
| Logging | `tracing`, `tracing-subscriber` | 0.1, 0.3 | Structured logs, `RUST_LOG` filtering. |
| Progress | `indicatif` | 0.18 | Scan, hash and pull progress in the CLI. |
| Index store | `native_db`, `native_model`, `redb`, `tantivy` | 0.8.2, =0.4.20, 2.6, 0.26 | Typed records with secondary indexes in one file; a rebuildable full-text index beside it; every write is one transaction. |
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
| Tests | `tempfile`, `wiremock` | 3.27, 0.6 | Temp trees, mocked upstream and peers. |

Not chosen and why:

- `rusqlite`: SQLite was the first index and was replaced by the native catalog. It takes
  one writer per file, its schema lives in SQL beside the types rather than in them, and its
  FTS5 index cannot be rebuilt without rewriting the rows it indexes; see
  `docs/superpowers/specs/2026-09-05-native-catalog-decision.md`. No SQLite code remains.
- `git2`: links libgit2; `gix` is pure Rust and read paths are all the index needs.
- `tantivy` alone: it is a search index, not a store. It is used beside `native_db`; see
  `docs/superpowers/specs/2026-09-05-native-catalog-decision.md` for why that pairing was
  chosen over SQLite FTS5 and over Fjall.
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
    native-catalog.md        the catalog's layout, modules and measured numbers
    handoff.md               repository state, the agentic loop, outstanding work
    api/hub-compat.md        HuggingFace Hub routes, derived from olah
    api/hoard-api.md         /api/v1 JSON contract for UI and CLI
    design/ui-spec.md        tokens, layout, columns, keys extracted from the design
    design/handoff/          Claude Design export, verbatim
    superpowers/specs/       decision records
    superpowers/research/    dependency and design research notes
    superpowers/history/     condensed record of finished work
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
    hoard-index/             discovery, sniffing, inference, the native catalog, search
      src/
        config.rs            IndexConfig: sources, globs, hashing policy, layouts
        coordinate.rs        the one lane every scan under serve runs on
        layout.rs            the ComfyUI or A1111 folder an entry's repository was filed under
        discover/
          walker.rs          ignore-based parallel walk
          hf_cache.rs        ~/.cache/huggingface/hub layout
          git_clone.rs       clones with LFS pointers
          loose.rs           bare folders and known tool layouts
        sniff/
          safetensors.rs     header and __metadata__ via bounded File reads
          gguf.rs            metadata via the hand-written v2/v3 header parser
          pickle.rs          zip listing for .ckpt/.pt/.pth/.bin
          markers.rs         the rule vocabulary found among every tensor name, one AC pass
          readme.rs          frontmatter and body
          dataset.rs         parquet, arrow, jsonl, csv
        infer/
          rules.rs           key prefix and shape signature tables, the marker vocabulary
          families.rs        a family by the exact keys ComfyUI detects it by, a LoRA by module
          role.rs  base.rs  lora.rs  precision.rs  trigger.rs
          lookup.rs          what a listing service's baseModel does to a stored base, no IO
        hash/                fingerprint, sha256, git blob sha1
        catalog/             the native_db + Tantivy catalog: the index
          mod.rs             Catalog open/status/integrity, format version, storage report
          codec.rs           the compact and cold record encodings native_model writes with
          models.rs          native_model records: Source, Repository, Revision, ArtifactRecord,
                             ArtifactDetail, Entry, SearchText, RevisionText, SearchChange,
                             Manifest, Summary, Allocator, Node, Usage, RepoUsage and the
                             user-data rows
          scan/mod.rs        discovery -> sniff -> infer -> one transaction per unit; CHUNK
          scan/counters.rs   Counters, DerivedShift, log_changes, the summary parts, ids
          scan/walk.rs       the walk state and the unchanged-or-changed decision
          scan/plan.rs       discovery to units to prepared entries: shard sets, roles, sniffing
          scan/store.rs      one entry, its artifact and its repository row
          scan/sweep.rs      the artifact sweep and every retirement
          scan/sources.rs    the source rows, their scanned counts, configuration validation
          scan/revisions.rs  discovered revisions merged with the stored ones
          naming.rs          the configs and cards a revision carries, each scoped to its own
                             directory, artifact names, file hashes
          search/mod.rs      QueryRequest, QueryPage, RefreshReport, the re-exports
          search/schema.rs   Fields, the keyword and column constants, the entry document
          search/build.rs    the rebuild, the metered writer, per-entry documents, publish
          search/refresh.rs  refresh_search, the change log and its cursor, the directory
          search/answer.rs   a query from request to page: path, facet rail, rows
          search/inspect.rs  segment, delete and document counts
          search/paging.rs   the page window, heap or sampled selection, boundaries
          search/tally.rs    per-segment facet tallies and the base rail buckets
          search/collapse.rs the per-artifact collapse, spelled terms, column blocks
          search/collectors.rs the Tantivy collectors: primary, sampling, bucket, window, count
          query.rs           the grammar compiled against the Tantivy index
          read.rs            inspector, repository screens, triage queue, sources
          verdict.rs         a user's override of what an artifact is
          lookup.rs          the artifacts a lookup may ask about and the writer of an answer
          seed.rs            synthetic records for tests, behind `fixtures`
          userdata.rs        shelves, settings, the noise list, projections
          usage.rs           the served-response log and per-artifact use counts
          loadouts.rs        loadout documents and pin resolution
          jobs.rs            scan, apply and pull job rows
          hashing.rs         sha256 and git blob sha1 per file, content identities, merges
          hash_memo.rs       <data_dir>/digests.redb: a digest against the stamp that earned it
          lookup_memo.rs     <data_dir>/civitai.redb: what a lookup was told about one sha256
          ingest.rs          a file the mirror fetched, filed as a served entry
          nodes.rs           this machine and every peer it knows
          digest.rs          the rows this node offers and the rows a peer advertises
          pulls.rs           a pull in flight and the file it lands
          tests/             shared fixtures in mod.rs; scan, revisions, storage, codec, reads,
                             mirror, counts, rescan, index, paging, collapse, usage, hashing,
                             recovery, brands, cards, naming, text, lookup, rejection
        row.rs               SearchRow, FacetBucket, DiskPressure: what a search answers with
        time.rs              reading and writing the instants the catalog stores
        search/
          grammar.rs         winnow parser
          token.rs           token kinds and the parsed strip
          context.rs         sort keys, compat targets, the context a query compiles against
          facets.rs          per-dimension counts excluding own key, disk pressure
          brands.rs          the brand a base root belongs to, for the rail's grouping alone
        shard.rs             groups a model split across files into one artifact, and names
                             the index document a set of that container ships
        triage.rs            queue, confirm, reject, noise list
        watch.rs             notify-driven rescan requests for the scan lane
      tests/scale.rs         ignored harness: 100k and 1M synthetic entries
    hoard-mirror/            serving faces and upstreams, transport-agnostic
      src/
        config.rs            MirrorConfig: upstreams, cache root, rules, offline, xet
        access.rs            proxy and cache rules
        batch.rs             batched writes and the shared search refresh a node debounces
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
          reconcile.rs       retires index rows whose cache directory is gone
          sweep.rs           the scheduled eviction plus reconciliation loop
        git/
          smart_http.rs      info/refs and upload-pack via the git binary
          lfs_batch.rs       LFS batch API pointing at resolve
        lookup.rs            the civitai by-hash worker, its config and its job
        resolve.rs           revision -> sha, local first
        ingest.rs            completed cache entries into the index
        usage.rs             served-request log and the sink that batches it
      tests/
        loose_revision.rs    the index answers a loose folder's sha without upstream
    hoard-net/               node identity, mDNS discovery, digest gossip, peer pulls
      src/
        identity.rs          NodeId, name, advertised URL and the _hoard._tcp TXT record
        discovery.rs         mDNS advertise and browse, with the static peer list as peer
        digest.rs            the compact index digest, its cursors and how one is applied
        peer.rs              the peer client, pulls through the Hub face, state derivation
        error.rs             NetError
    hoard-server/            axum wiring only
      src/
        state.rs  config.rs  error.rs  layers.rs  assets.rs
        testing.rs           test-only fixtures
        hub/                 Hub-compatible routes: mod.rs meta.rs files.rs proxy.rs
        git/                 git smart HTTP and LFS routes (mod.rs)
        api/                 /api/v1: search.rs compat.rs artifacts.rs repos.rs shelves.rs
                             loadouts.rs triage.rs jobs.rs nodes.rs link.rs system.rs
                             common.rs common_tests.rs
        ui/                  embedded web/ and index.html fallback
  web/
    index.html
    css/tokens.css  css/hoard.css  vendor/pico.min.css  fonts/
    js/
      app.js  api.js  dom.js  search.js  facets.js  table.js  cards.js  inspector.js
      detail.js  readme.js  loadout.js  shelves.js  nodes.js  jobs.js  keys.js  theme.js
```

End-to-end tests live beside the code they cover, in `tests.rs` modules under
`crates/hoard-server/src/{api,hub,git}/`, driven by the `fixtures` feature of `hoard-index`.
A `tests.rs` that has grown enough takes child modules beside it that reach its helpers
through `super::`: `api/tests/opened.rs` covers the repository routes of an opened artifact.

Dependency direction is strict: `hoard-core` <- `hoard-index` <- `hoard-mirror` <-
`hoard-net` <- `hoard-server` <- `hoard`. No crate below `hoard-server` knows about axum.

## Data flow

Scan:

```
sources -> discover -> sniff (headers only) -> fingerprint -> infer (role, base, rank,
precision, trigger, confidence) -> catalog; sha256 in a background job; low confidence
-> triage; the search index is refreshed from the change log the same run wrote
```

Under `serve` every scan goes through `coordinate::Coordinator`: a request answers its job
id at once and the lane runs the rescan, the hashing and the refresh one request at a time,
merging a request for a source set that is queued and has not started into it.

`watch::Watcher` is the second producer for that lane. It watches every configured source
root, drops events under the catalog and cache roots, coalesces the rest over a debounce
window and submits one request per burst; an overflow, a signal it could not queue, or its
reconciliation timer submits one over every source instead. Shutdown joins the watcher
first, then stops the lane, then flushes the usage sink and the search refresher, so
nothing is still submitting when the lane closes. Stopping the lane asks the running scan
to stop before each source it has not discovered yet, between the units of the one it is
inside and between hashing batches, and cancels what has not started; the next start rescans
what neither finished. An exit therefore costs a batch, a unit, or the discovery walk of the
one source it is inside, and never a whole pass.

Serve, local hit on the Hub face:

```
GET /org/name/resolve/main/file.safetensors
  -> resolve revision (index) -> entry -> blob path
  -> stream with Range, x-repo-commit, etag = "sha256", x-linked-size
  -> attribute the path through the catalog -> catalog usage log + artifact use count
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

Index screen search:

```
GET /api/v1/search?q=... -> Catalog::list_shelves -> Catalog::compile_context
  -> Catalog::query on a blocking thread -> page, totals, six facet dimensions
```

`GET /api/v1/stats` and `hoard ls` read the same catalog, and so do the artifact inspector,
compat, pull command, override and blob views, the repository list, detail, files, card and
compat, the triage queue with its confirm and reject, the shelves and settings routes, the
noise list a rejection writes, the projections `hoard link` and `hoard apply` record, and
the usage the Hub logs and the USAGE tab reads, the loadouts every loadout route and
`hoard loadout` store and resolve, every job a scan, apply or pull runs under, and the
sources listing with the counts its last scan wrote and the files it still owes a hash. The
catalog hashes its own files, inline when the scan asks for it and by a background pass
otherwise. The Hub and git faces read it too, and the mirror holds it rather than the store:
a file it fetches from upstream is filed there as a served entry. `hoard-net` holds the
catalog as well: every node, the digest it offers, the rows a peer advertises and the bytes
a pull lands are catalog records.

Nodes:

```
mDNS _hoard._tcp -> node records -> periodic GET /api/v1/peers/digest?since=cursor
  -> remote entries in the catalog -> `hoard apply` pulls via the proxy path
  -> hourly full re-read retires the rows a peer stopped naming
```

## Storage

- Index: `<data-dir>/native/`, recreatable from disk plus peers. It holds `FORMAT`, the
  format stamp read before redb is opened, `catalog.redb`, the one file every record lives
  in, and one `search-v1-<nanos>/` directory holding the
  published Tantivy index; a rebuild writes a new directory and the old ones are removed
  once the manifest names the new one. Beside the scanned sources, repositories, artifacts,
  entries and search text the redb file holds the user data those screens read: shelves (the
  six builtin ones seeded on every open), the shell's settings, the noise list a rejection
  writes and the scan walks past, the projections a link records, the usage log every served
  response appends to with the per-artifact use count `used:` and the last-used column read,
  the loadout documents the drawer edits, and the job rows a scan, apply or pull writes.
  Every command that opens the index opens or creates it there, and `hoard scan` and the
  `/api/v1/scan` job rescan and refresh it; the server holds it as shared state. `hoard
  catalog` still takes any directory with `--directory`. A catalog has one format,
  `catalog::FORMAT_VERSION`, and a directory written in another is removed and scanned again
  rather than converted; the "Compatibility" section of `CLAUDE.md` is the policy, and
  `docs/native-catalog.md` describes the layout and the modules.
- Mirror cache: `<data-dir>/repos/` in the olah layout (`docs/api/hub-compat.md`).
- Projections: symlinks written by `hoard link` and `hoard apply`, tracked in the native
  catalog so PATHS ON DISK can name the node and the link kind.
- Config: `hoard.toml`; secrets only in `.env` (`HF_TOKEN`, `HOARD_NODE_TOKEN`).
