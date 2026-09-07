# Hoard feature plan

Hoard turns weights scattered across disks and machines into one searchable index, and
serves that index to every machine on the LAN through the protocols the tools already
speak. Sources: `docs/use-case.md` (brief), `docs/design/ui-spec.md` and
`docs/design/handoff/` (design), `docs/architecture.md` (stack), `docs/api/hub-compat.md`
(Hub contract), `docs/api/hoard-api.md` (UI/CLI JSON API).

## Phase status

| Phase | Name | Status | Design milestone |
| --- | --- | --- | --- |
| 0 | Foundations | done | |
| 1 | Core types and index store | done | M1 Index locally |
| 2 | Discovery and sniffing | done | M1 |
| 3 | Inference, triage and search | done | M1 |
| 4 | Index screen (web UI) | done | M1 |
| 5 | Hub face, local serving | done | M2 Speak git |
| 6 | Git face and repo detail | done | M2 |
| 7 | Upstream proxy and block cache | done | M3 Cross the LAN |
| 8 | Nodes: discovery, gossip, peer pulls | done | M3 |
| 9 | Shelves, loadouts, projection, usage | done | M4 Make it usable |
| 10 | Live index and polish | done | M4 |
| 11 | Native catalog (native_db + Tantivy) | done | M1 |

Tick a phase only when its acceptance check passes and `/hoard-verify` is clean.

Phases 1 to 7 were accepted against this workstation's real disks, not fixtures: 1.5 TB
across the HuggingFace cache, a ComfyUI tree and the LM Studio models; `hf download`
byte-identical through `HF_ENDPOINT` with an isolated `HF_HOME` so the client could not
answer itself; `git clone` with branches, tag and history intact; and a second node in
offline mode serving a proxied repo from the cached documents alone.

Phase 8 was accepted with two nodes on separate data directories: mDNS found each other
within a second on this network, the second listed the first's artifacts as REMOTE, and an
apply pulled one whose sha256 matched the origin byte for byte before it became local and
searchable. The static `[peers]` path was verified independently, because mDNS is blocked
on many networks.

Phase 10 is done: PyTorch zip key extraction, dataset sniffing, the `?` sheet, the doctor
command and live rescanning. `hoard serve` watches every configured source by default,
submits what changes to the scan lane under the configured hash policy, reconciles every
source after an overflow and on an interval, never rescans on its own catalog and cache
writes, reports the watch in `/healthz`, and joins the watcher before the lane drains. The
TREE and DISK views are not part of the phase: they need a design decision before they can
be built, and are recorded under "Risks and open decisions".

Phase 11 is done. It replaced the SQLite index with the native catalog: every command, every
route, both faces and gossip read and write it, and no SQLite code remains, which is what the
phase was for. Making the catalog faster and smaller is ongoing work rather than an unfinished
phase, and the follow-ups under `### Phase 11` carry their own acceptance criteria.

## Goals

1. Index every model, dataset and repository file on nominated disks without moving,
   renaming or rewriting anything. The filesystem stays authoritative; meaning lives in the
   index.
2. Identify each artifact from its bytes: format, role, base architecture, rank, precision,
   quantization, trigger words. Filenames and folders are hints confirmed against tensors.
3. Deduplicate by content hash so one blob shows every path it has, on every node.
4. Search with one grammar used identically by the search bar, CLI, HTTP API and shelf
   definitions, with facets that never dead-end and a directional `compat:` query.
5. Serve three compatible faces on the LAN: the HuggingFace Hub API (`HF_ENDPOINT`),
   git over HTTP with LFS, and a filesystem projection of symlinks in the layout each tool
   expects. Proxy and block-cache `huggingface.co` for repos not held locally.
6. Nodes: every install is a node; nodes find each other on the LAN, gossip their index,
   and pull from each other peer to peer, resumable and hash-verified.
7. Loadouts: checkpoint plus adapters at weights plus auxiliaries, pinned by commit,
   portable as a text file, applied to a node as symlinks.

## Non-goals (from the proposal's OUT list)

- Inference, quantization, conversion or training.
- Accounts, orgs or permissions beyond a per-node write token.
- Any cloud service, sync account or telemetry.
- Uploading to public hubs from the UI.
- Mobile or tablet layouts.
- A storage format of its own.

## Primitives

| Primitive | Meaning | Identity |
| --- | --- | --- |
| Node | A machine running hoard; the local node plus discovered peers | `NodeId` (stable uuid), name |
| Source | A nominated root on a node: HF cache, git clones, loose folder | path |
| Blob | Unique bytes | sha256 (LFS parity), git blob sha1 for small files, xxh3 fingerprint for candidates |
| Artifact | A blob interpreted: format, tensor summary, role, base, rank, precision, confidence | blob + sniff version |
| Repo | `type/org/name`: a git clone, a Hub cache repo, or a bare directory adopted as one | `RepoId` |
| Revision | A commit, branch or tag; loose repos get one synthetic sha from their content | `CommitSha` |
| Entry | A path inside a revision pointing at a blob, on a node | (revision, path, node) |
| Role | ckpt, lora, ctrl, upsc, vae, emb, llm, ds, unk; sub-roles ipa, tok, mot | inferred, reviewable |
| State | served, local, pulling, stale, remote, triage | derived per entry and node |
| Shelf | A named query | query string |
| Loadout | Pinned entries with weights | TOML document |

## Feature inventory

### A. Discovery and indexing (`hoard-index`)

- A1 Sources in `hoard.toml`: path, kind (`hf-cache`, `git`, `loose`, `auto`), include and
  exclude globs, `.hoardignore`, symlink policy. Known tool layouts (ComfyUI, A1111,
  kohya, llama.cpp) are recognized as loose sources with folder hints.
- A2 Walk with `ignore::WalkBuilder` in parallel; target the proposal's 180k files per
  minute; progress via `indicatif` in the CLI and `/api/v1/jobs` for the UI SCANS tab.
- A3 HF cache source: `models--org--name/refs/*`, `snapshots/<sha>/` symlinks,
  `blobs/<hash>`; blob names already are sha256 or git sha1.
- A4 Git source: `gix` reads refs, trees, blobs; LFS pointers resolve to
  `.git/lfs/objects`; missing objects index as `LFS PTR`.
- A5 Loose source: one synthetic repo per top-level folder (configurable), one revision
  derived from content hashes, entries by relative path.
- A6 Sniff by magic and header only: safetensors, GGUF, PyTorch zip (`.ckpt`, `.pt`,
  `.pth`, `.bin`), ONNX magic, diffusers dir (`model_index.json`), transformers dir
  (`config.json`), dataset files (parquet, arrow, jsonl, csv, gz), README.
- A7 Tensor summary per artifact: count, params, dtype histogram, key prefixes, sample of
  keys with shapes, safetensors `__metadata__` (kohya `ss_*` fields give rank, alpha, base
  and trigger words).
- A8 Hashing: xxh3 fingerprint at scan time; sha256 in a background rayon pool, lowest
  priority, resumable, cached by inode and mtime; git blob sha1 for files under the LFS
  threshold.
- A9 Duplicates: same sha256 is one blob with many paths; `dupe:true` lists them; the
  DISK PRESSURE facet reports reclaimable bytes; deletion only via explicit confirmed
  action.
- A10 Incremental rescans with `notify` and mtime/size short-circuit; missing files mark
  entries absent rather than deleting rows.
- A11 Store: a native_db (redb) catalog with a Tantivy search index; one process per
  catalog, every write one transaction, all through `spawn_blocking`.
- A12 Enrich (offline-safe): when a Hub cache or clone names the upstream repo, recover
  card, license and revisions from the local README and refs; never a network call
  during scan.

### B. Inference and triage (`hoard-index::infer`)

- B1 Role from tensor keys (lora_unet_/lora_te_/.lora_down = lora; control_model. = ctrl;
  first_stage_model. or decoder.up_blocks alone = vae; model.diffusion_model. with text
  encoders = ckpt; string_to_param or emb_params = emb; ESRGAN/SwinIR bodies = upsc;
  GGUF general.architecture = llm; ip-adapter keys = lora sub-role ipa; motion module
  keys = ctrl sub-role mot; tokenizer files = emb sub-role tok).
- B2 Base from shapes and configs: cross-attention dim 768 (sd15), 1024 (sd2), 2048
  (sdxl), Flux double/single stream, Wan, GGUF architecture, transformers `model_type`.
  Stored with confidence and the evidence key.
- B3 Rank and alpha for LoRA; precision from dtype histogram; GGUF file type name.
- B4 Confidence below threshold puts the artifact in the triage queue (`role:?`, state
  TRIAGE) with the best guess and the header facts the design's CARD tab shows for
  unclassified items. Confirm assigns role and base; reject records the path as noise so
  future scans skip it. Overrides survive rescans.
- B5 Compatibility is directional (see `docs/design/ui-spec.md`): adapters (lora, ctrl,
  vae, emb, upsc) resolve against hosts (ckpt, llm) with the same base or base `any`;
  never adapter to adapter, never datasets.
- B6 Every inferred field records its source (tensor, config, metadata, frontmatter,
  folder hint, user) for the DERIVED FACTS panel.

### C. Search (`hoard-index::search`)

- C1 Grammar exactly as prototyped: whitespace tokens with quoted values, `key:value`,
  comma OR, `-` negation, `~` fuzzy, free text. Keys: role (with alias table), base, fmt
  and format, prec and precision, node, state, rev and tag, dupe, compat, size with
  `<`/`>` and kb/mb/gb/tb, used with `>30d`, trigger, plus repo, org, source, hash, shelf.
- C2 Parse endpoint returns the token strip (key, value, kind, normalized value, warning).
- C3 Facets ROLE, BASE ARCHITECTURE, FORMAT PRECISION, NODE, each computed with its own
  key removed; DISK PRESSURE summary.
- C4 Sort by any column plus `used`; keyset pagination.
- C5 Shelves are saved queries; the six from the prototype are seeded as defaults.

### D. Hub face (`hoard-mirror`, `hoard-server::hub`)

- D1 Local serving from the index: repo info, tree, paths-info, commits, resolve with
  Range and multi-range, HEAD parity, ETag and LFS headers, listing endpoints.
- D2 Git clone serving via `gix` including LFS objects.
- D3 Upstream proxy with the olah block cache format, single-flight block downloads, CRC
  verification, eviction scheduled hourly and at startup with index reconciliation, access
  rules, offline mode, visibility probe with retry classification.
- D4 Ingest completed cache entries into the index so cached files are searchable and
  deduplicated.
- D5 Xet: strip `x-xet-hash` on served responses, proxy token and bridge routes.
- D6 Default port 7099 (`http://hoard.lan:7099` in the design).

### E. Git face (`hoard-mirror::git`)

- E1 Read-only smart HTTP: `GET /{org}/{repo}[.git]/info/refs?service=git-upload-pack` and
  `POST .../git-upload-pack`, implemented by spawning `git upload-pack --stateless-rpc`
  with `tokio::process` over the clone directory. Only git sources are clonable; cache
  and loose repos answer 404 on the git face and remain reachable via the Hub face.
- E2 LFS batch API `POST /{org}/{repo}.git/info/lfs/objects/batch` returning download
  hrefs that point at the Hub `resolve` route by sha256, so `git lfs` works over the LAN.
- E3 Push is refused unless the request carries the node write token; MVP ships read-only.

### F. Nodes (`hoard-net`, `hoard-mirror::upstream`)

- F1 Node identity: stable id, name, advertised URL; `[node]` section in `hoard.toml`.
- F2 Discovery with mDNS/DNS-SD (`_hoard._tcp`); manual peers list as fallback.
- F3 Gossip: peers exchange compact index digests (blob sha256, size, role, base, repo,
  revision, paths) over `/api/v1/peers/digest` with incremental cursors; the local index
  stores remote entries with state REMOTE and the owning node.
- F4 Peer pulls reuse the upstream proxy: a peer is an upstream that speaks the Hub API,
  so a pull is a block-cached resolve from `http://<peer>:7099` with hash verification on
  completion. Pulls show as PULLING nn% in the table and in JOBS.
- F5 States derive per entry: SERVED (local and served), LOCAL (local, not served),
  REMOTE, PULLING, STALE (pinned sha differs from the ref on its origin), TRIAGE.
- F6 Read-only by default; writes need the node token; served paths are streams, never
  mounts.

### G. Web UI (`hoard-server::ui`, `web/`)

- G1 Shell per `docs/design/ui-spec.md`: top bar with nav tabs, search, node pulse,
  loadout button; token strip; facet rail; results header with TABLE and CARDS; nine
  column table with Condensed and Roomy densities; footer key hints; inspector rail.
- G2 Keyboard map as specified (`/`, j/k, Enter, Space, L, S, Y, Esc, `[`, `]`, `?`).
- G3 Repo detail with FILES, REFS, CARD, COMPAT, USAGE tabs, revision bar with SWITCH,
  right rail with PULL TO THIS MACHINE, DERIVED FACTS, PATHS ON DISK.
- G4 CARD renders README from the clone at the chosen revision with YAML frontmatter
  separated; generated card from the index record when no README exists.
- G5 Loadout drawer with weights, remove, apply command preview and APPLY TO node.
- G6 Toasts for pin, yank, serve, checkout, apply. Settings density, roleColorMode,
  showInspector, theme persisted in `localStorage`.
- G7 SHELVES, NODES, SCANS, JOBS tabs (list views over their `/api/v1` endpoints).
- G8 Later: TREE view (repo, revision, file) and DISK treemap by node.

### H. Shelves, loadouts, projection, usage

- H1 Shelves CRUD, seeded defaults, pinned order.
- H2 Loadout TOML: name, host, adapters with weights, vae, controlnets with weights,
  upscaler with factor, embeddings; each pinned by repo, path, sha and sha256.
- H3 Resolution by sha256 first, then repo+sha+path; status per line: present,
  elsewhere (on another node), missing; validation with the compat rules.
- H4 `hoard apply <loadout> --node <name>`: pull what is missing from peers, then write
  symlinks in the target tool's layout. `hoard link --profile comfyui|a1111|llama <dir>`
  projects a query or shelf the same way.
- H5 Usage log: every served request records time, client, path, bytes, rate; feeds the
  USAGE tab (7 day bars, recent pulls), `used:` queries, `last used` in headers and the
  `pulls on lan` fact.

### I. CLI (`hoard` binary)

- `hoard serve [--config] [--bind 0.0.0.0:7099]`
- `hoard scan [--source name] [--hash now|later|never]`
- `hoard ls "<query>" [--json]` and `hoard search` as an alias
- `hoard path <repo-or-artifact>` prints the on-disk path (used in the design's llama-server
  example)
- `hoard loadout {show,resolve,export,import,validate} <file>`
- `hoard apply <loadout> --node <name>`
- `hoard link --profile <name> <dir>`
- `hoard nodes` and `hoard doctor`

### J. Operations

- `hoard.toml` sections: `[server]`, `[node]`, `[index]` with `[[index.sources]]`,
  `[mirror]`, `[[mirror.proxy]]`, `[[mirror.cache]]`, `[peers]`. Secrets only via `.env`
  (`HF_TOKEN`, `HOARD_NODE_TOKEN`).
- `GET /healthz`, request ids, structured logs, timeouts, body limits, compression only for
  text responses, graceful shutdown that finishes block writes.

## Phases and acceptance

### Phase 0: Foundations (done)

Workspace, four crates, pinned dependencies, lints, vendored Pico CSS, design tokens,
docs, Claude Code project configuration.

### Phase 1: Core types and index store

Its first implementation was SQLite with an FTS5 index; Phase 11 replaced that store with the
native catalog, and the bullets below describe what the catalog holds.

- `hoard-core`: `RepoId`, `RepoType`, `CommitSha`, `Sha256`, `BlobId`, `ArtifactId`,
  `NodeId`, `Role` with sub-roles and aliases, `Base`, `ArtifactFormat`, `Precision`,
  `State`, `Evidence`, `HoardError`.
- `hoard-index::catalog`: the `native_model` records for node, source, artifact, tensor
  summary, repository, revision, entry, override, noise, shelf, loadout, job and usage, the
  secondary indexes over them, and typed request and response types so no caller names a
  native_db or Tantivy type. The record-shape policy is the "Compatibility" section of
  `CLAUDE.md`.
- Accept: unit tests round-trip a synthetic repo with two revisions and a duplicate blob and
  survive a process reopen; role alias table parses every alias from the design.

### Phase 2: Discovery and sniffing

- Walker, HF cache source, loose source with layout hints, safetensors and GGUF header
  sniffing including kohya metadata, fingerprint and background sha256, `hoard scan`.
- Accept: scanning `~/.cache/huggingface/hub` on this workstation indexes every snapshot
  reading only headers; a rerun with no changes finishes under one second per thousand
  files; the design's `sd_xl_refiner_FINAL_v2.safetensors` case (no README, no config)
  lands in triage with a header summary.

### Phase 3: Inference, triage and search

- Role, base, rank, precision, trigger words with evidence and confidence; triage confirm
  and reject; grammar parser; compilation to a Tantivy query; facets; compat; `hoard ls`.
- Accept: synthetic header fixtures for sd15, sdxl, flux and wan checkpoints, LoRAs per
  base, a VAE, a ControlNet, a motion module, an embedding, an ESRGAN upscaler, a GGUF
  llm and an ip-adapter classify with the expected role and base; every grammar row in
  `ui-spec.md` parses; `compat:sdxl-base-1.0 role:lora` never returns an adapter for an
  adapter; each facet count equals the count the click produces.

### Phase 4: Index screen

- `hoard-server` app shell, `/api/v1/search`, `/parse`, `/artifacts`, `/shelves`,
  `/triage`, embedded `web/` with the index view, inspector, cards, keyboard map, theme.
- Accept: pixel dimensions match `ui-spec.md`; every key in the map works; facet clicks
  never empty the table; light and dark both render with the role colors.

### Phase 5: Hub face, local serving

- `hoard-mirror::local`, `hoard-server::hub`, Range handling, error contract, listing
  endpoints, `/healthz`, usage log.
- Accept: with `HF_ENDPOINT` pointed at hoard, `hf download` and `snapshot_download` of a
  cached repo complete byte-identical; integration tests cover HEAD/GET parity and the
  401/404/416 codes.

### Phase 6: Git face and repo detail

- Git source with LFS, smart HTTP upload-pack, LFS batch, detail view tabs FILES, REFS,
  CARD, COMPAT with revision switching, README rendering, PATHS ON DISK.
- Accept: `git clone http://127.0.0.1:7099/org/repo` of an indexed clone succeeds with
  refs and LFS objects intact; the CARD tab renders a README with frontmatter separated
  and a generated card for a repo without one.

### Phase 7: Upstream proxy and block cache

- Access rules, visibility probe, metadata cache, block cache format, single-flight
  downloads, eviction on an hourly schedule, offline mode, ingest, Xet handling.
- Accept: `wiremock` upstream tests for ranged block fetches, CRC invalidation and offline
  fallback; an existing olah `repos/` directory serves its cached files unchanged.

### Phase 8: Nodes

- `hoard-net` crate: identity, mDNS discovery, digest gossip, peer pulls through the proxy,
  states, NODES tab, node facet, PULLING progress in JOBS.
- Accept: two hoard instances on one machine with different ports discover each other,
  the second lists the first's artifacts as REMOTE, and `hoard apply` pulls one with
  verified sha256.

### Phase 9: Shelves, loadouts, projection, usage

- Loadout document, drawer, resolution and validation, `hoard apply`, `hoard link`
  profiles, USAGE tab, `used:` key.
- Accept: a loadout exported here resolves on the second instance; symlinks land in a
  ComfyUI layout; an adapter never lists another adapter as compatible.

### Phase 10: Live index and polish

- PyTorch zip key extraction, ONNX and dataset sniffing, the `?` key sheet, and the doctor
  command, which diagnoses by default and repairs the derived totals on `--repair`.
- The `notify` watcher through `serve`: started with the server, reconciling what changed
  while it was down, and joined on shutdown.
- Accept, and each holds: a file added under a configured source while `serve` runs is
  searchable without a manual scan; a manual scan and a watched scan never run over the same
  source at once, because both are requests on `coordinate::Coordinator`; the configured
  hash policy is preserved rather than forced; an overflowed or lost watch marks the sources
  dirty and reconciles every one of them instead of losing the change; a timer reconciles on
  `index.reconcile_interval_secs` regardless, starting with the tick that covers what changed
  while the node was down; Hoard's own catalog and cache writes never trigger a rescan; a
  source that cannot be watched is `degraded` in `/healthz` while manual scans keep working;
  and shutdown joins the watcher before the lane it feeds is drained, so it leaves nothing
  running.

### Phase 11: Native catalog

- `hoard-index::catalog`: the native_db records, the Tantivy index built and refreshed from
  them, the grammar compiled against it, and every reader and writer moved onto it. The
  SQLite store and its FTS5 index are gone.
- Accept: a scan of this workstation's disks yields the same artifact count, names and triage
  set the SQLite index gave; `hoard doctor` passes every check; the scale harness meets its
  thresholds at 1M entries. All three hold, and the phase is done.

Follow-ups, each with its own acceptance criterion. They are optimisation and hardening of a
store that already works, not the remainder of the phase.

- Bounded collapse. Accept: every query that takes the collapse path answers under 500 ms at
  1M with memory bounded by the page and the sample, as the primary path already is.
  Implemented in wave 4, which folds each artifact's run of entry documents inside the
  segment collectors; the acceptance is the final measurement's job in wave 8.
- Power-loss and process-kill recovery tests. Accept: a catalog killed mid-scan reopens and
  rescans to the same counts a clean run gives. **Done:** wave 6 interrupts the scan, the
  hash pass, the refresh and the rebuild from the progress callback each already takes, and
  completes each of them on both what the kill left and a byte copy taken at that instant. It
  fixed what the tests found: a fresh scan with no manifest that no open would rebuild, ids
  handed out twice after a kill, derived counts left behind by one, a log row the cursor had
  passed that no refresh dropped, and the directory a killed rebuild left.
- Byte-exact path identities, so an identity is the path's bytes rather than its lossy string.
- Revision ordinals stored rather than recomputed on every rescan.
- Counting a revision's artifact units without counting content another revision claimed.
- Complete repository git metadata with identities that are stable across scans. **Done:**
  wave 5a keeps revision ordinals, unit names and path identities stable across rescans, and
  wave 5b records the remote URL a clone came from, which was the one fact discovery read
  and dropped.

`docs/native-catalog.md` under "Remaining work" holds each of these with its measured numbers.

## Risks and open decisions

- PyTorch `.ckpt` inference without unpickling yields storage names only. Plan: walk the
  pickle opcode stream for tensor keys and shapes without executing it; until then such
  files get role from folder hints at low confidence and go to triage.
- Git face depends on a `git` binary on the node; `gix` does not serve upload-pack.
  Acceptable for MVP, documented in `hoard doctor`.
- Hashing hundreds of GB takes hours; fingerprint-first indexing keeps the UI usable and
  Hub `resolve` for a loose file returns 404 until its sha256 exists, since clients verify
  ETags.
- mDNS may be blocked on some networks; the manual `[peers]` list is the fallback.
- The catalog takes an exclusive lock: one process at a time, which `hoard doctor` reports
  when a second one tries.
- TREE and DISK views are blocked on a design decision, not on implementation. There is no
  layout for them in `docs/design/handoff/`: `docs/design/ui-spec.md` records them as named
  in the proposal but absent from the prototype, so what they show has to be decided before
  either can be built.
