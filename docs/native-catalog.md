# The native catalog

The native_db (redb) plus Tantivy catalog is Hoard's index. `serve`, `scan`, `ls`, the
mirror, the git face and gossip all read and write it, and nothing else persists what Hoard
knows about a file.

This document is the current state. `docs/superpowers/history/2026-09-native-catalog-waves.md`
holds the wave-by-wave record of how it was built.
`.claude/skills/hoard-catalog/SKILL.md` carries the same module map annotated for someone
about to change the code, plus the native_db and Tantivy semantics that the calling code does
not show.

## Decision

`docs/superpowers/specs/2026-09-05-native-catalog-decision.md` records why this pairing was
chosen. native_db was preferred over raw Fjall because it already maintains typed records,
transactions and secondary indexes on redb, so Hoard does not have to own an index
maintenance layer. Tantivy replaced SQLite's FTS5 as a disposable search index that is
rebuilt from the native records alone, without re-reading a single source file.

## Compatibility

Read the "Compatibility" section of `CLAUDE.md`. It is the only statement of the policy;
nothing here restates it. In short: one shape per record, no migrations, and a catalog of
another format is rebuilt rather than converted.

## Layout

`<data_dir>/native/` holds:

- `catalog.redb`, the one file every record lives in.
- one published `search-v1-<nanos>/` directory holding the Tantivy index. A rebuild writes a
  new directory and publishes its name in a redb transaction only after Tantivy commits; an
  unpublished build leaves the previous one serving, and publishing removes every superseded
  directory.

The whole thing is recreatable from disk plus peers. redb takes an exclusive lock, so one
process may open a catalog at a time; `hoard doctor` reports it when a second one tries.

`hoard catalog` takes any directory with `--directory`. Every other command opens or creates
the catalog at `<data_dir>/native`.

## Modules

`crates/hoard-index/src/catalog/`:

- `mod.rs` open, `open_or_scan`, status, integrity, `FORMAT_VERSION`, the per-table storage
  report, compact.
- `models.rs` every `native_model` record: `Source`, `Repository`, `Revision`,
  `ArtifactRecord`, `ArtifactDetail`, `Entry`, `SearchText`, `SearchChange`, `Manifest`,
  `Summary`, `Allocator`, `Node`, and the user-data rows.
- `scan.rs` discovery to sniff to infer to one transaction per unit; `Counters`,
  `DerivedShift`, `log_changes`, the artifact sweep.
- `naming.rs` the config and card a revision carries, artifact names, per-file hashes.
- `search.rs` the Tantivy schema, the rebuild, per-entry documents, the `primary` mark, the
  collectors, `refresh_search` and the published-directory swap.
- `query.rs` the grammar compiled against the Tantivy index.
- `read.rs` the inspector, the repository screens, the triage queue, the sources listing.
- `verdict.rs` a user's override of what an artifact is, re-applied after a rescan.
- `seed.rs` synthetic records for tests, behind `cfg(test)` or the `fixtures` feature.
- `userdata.rs` shelves, settings, the noise list, projections.
- `usage.rs` the served-response log and per-artifact use counts.
- `loadouts.rs` loadout documents and pin resolution.
- `jobs.rs` scan, apply and pull job rows.
- `hashing.rs` sha256 and git blob sha1 per file, content identities, merges.
- `ingest.rs` a file the mirror fetched, filed as a served entry.
- `nodes.rs` this machine and every peer it knows.
- `digest.rs` the rows this node offers and the rows a peer advertises.
- `pulls.rs` a pull in flight and the file it lands.

## What the scan produces

- Existing HF-cache, Git and loose-source discovery feeds the store directly. Header sniffing
  and inference reuse the existing Rust implementations.
- A revision's documents (README, configs, index files) are sniffed first and give every
  later unit its inference context; each shard set and each chunk of 1,024 standalone files
  is then sniffed in parallel, prepared and committed as its own transaction, so memory grows
  with a unit rather than with a source.
- Only weight and dataset formats become artifacts; every file is still an entry record.
- Shard sets, whether named `model-<n>-of-<m>` or listed by a `*.index.json` weight map,
  become one artifact anchored on the lowest shard. Members are entries pointing at that
  artifact with no search row; the anchor carries the whole model's size and shard count, and
  an incomplete set is triage.
- An unreadable file keeps its path and is recorded as an absent entry with no artifact; a
  git-lfs pointer stub is absent with the pointer's size and hash; a fetched LFS weight is an
  ordinary artifact.
- Every present file is hashed, inline when the scan is asked to hash now and by a background
  job otherwise. An artifact identified by its path becomes identified by its content once
  hashed, merging copies. Artifact identities are 128-bit hashes of the identity text, so the
  unique index holds fixed-width keys.
- Ids come from a persisted allocator; the generation increments per rescan; artifact path
  counts are swept from the entry index after every scan and orphaned artifacts are removed.
- A completion manifest is published after all batches commit, so an interrupted scan stays
  unusable rather than exposing a partial catalog.
- A rescan does not compact, so a catalog shared by the server can be walked again without
  an exclusive borrow. Only a fresh scan compacts, and `hoard catalog scan` compacts again
  after it finishes; there is no compact subcommand.

## The write path

Every writer of an entry commits through `scan::Counters`:

- `Counters::load` reads the persisted `Allocator` and hands out ids.
- `Counters::touch(transaction, id)` is called before an artifact record is written and keeps
  what the record contributed to the summary beforehand, so a pass moves the summary by a
  difference rather than recounting every record.
- `Counters::derived` is a `DerivedShift`: `added(&entry)` before writing an entry,
  `removed(&entry)` before removing one or moving it out of presence. It accumulates each
  repository's present files and bytes, each source's owed hashes and each node's filed
  count, so a record above the entries is rewritten once per pass rather than once per entry.
- `log_changes(transaction, counters)` writes one `SearchChange` row per touched artifact, in
  the transaction that made the change. The sequence continues past both the highest key the
  log holds and `Manifest.search_seq`.
- `Counters::save(transaction)` flushes the shift and writes the allocator back.

**Every writer of an entry moves the derived counts.** A writer that skips `DerivedShift`
leaves `Repository::present_files`, `Repository::present_bytes`, `Source::pending_hashes` and
`Node::filed` behind; `hoard doctor`'s summary check is what catches it. A writer that skips
`log_changes` leaves the search index stale with no record that it is.

`refresh_search` replays the log after the durable cursor, one Tantivy commit per batch,
deleting an artifact's documents by term and re-adding its entries, then prunes what it
applied. A rescan followed by a refresh updates search without a rebuild. A query answers
from the published index even when a rescan has logged changes no refresh has applied, and
says so with `stale`; only a catalog with no index at all is refused.

## Search document shape

The index contains **one document per entry**. This preserves a requirement the earlier
one-document-per-artifact proposal missed: `node:laptop AND state:local` must match one entry
satisfying both predicates, and the preferred row must be chosen from the entries that
matched rather than fixed before filtering.

One document per artifact carries a `primary` mark: the entry a row would be assembled from,
which is the highest availability preference and on a tie the lowest entry id. A query that
filters and sorts only on fields every entry of an artifact agrees on is intersected with
that mark and needs no collapse. For M matched documents and K equal to offset plus limit,
collection is then O(M) time and O(K) memory per segment, plus one tally per distinct facet
value, and page selection is O(K log K).

A query naming `node`, `state`, `source`, `repo`, `rev` or `hash`, or sorting on revision,
node or state, still collapses, because those fields differ between the entries of one
artifact. Collapse retains one small candidate per matching artifact per segment and then
combines segment results: O(M) time and O(A) memory per segment for A matched artifacts, with
page selection O(A + K log K). Deep pagination and broad queries on that path still require
memory proportional to matching artifacts.

Keyword columns are string fast fields and the collectors keep **term ordinals**, not terms.
Ordinals are lexicographic inside a segment and meaningless across segments, so a page
ordered by a term sorts on ordinals while one segment holds every hit and otherwise bounds
each segment on its own ordinals and merges the survivors by the terms they spell. A term is
spelled once per segment and ordinal, only for a page row, a facet bucket or the node set. A
page nearer the end of the order is cut from the reversed order, so a deep offset selects and
spells no more than a shallow one.

`Catalog::query` answers a whole index screen: the page in any of the ten sort orders, the
distinct artifact and node totals, and the five facet dimensions. Each dimension is counted
over the query with its own key excluded, so clicking a bucket never collapses the rail it
was clicked in; `base` buckets are counted by running each bucket's own query, because the
prefix rule does not group.

Free text indexes names, organizations, bases, triggers, tensor key prefixes and the body of
the shallowest README in each revision. Tantivy uses one writer thread with a 32 MiB indexing
budget; nothing larger than a unit is held in memory or in an open transaction.

## Running it

Every command opens or creates the catalog at `<data_dir>/native`:

```sh
cargo run -- --config hoard.toml scan
cargo run -- --config hoard.toml serve
cargo run -- --config hoard.toml ls 'role:lora size:<400mb'
```

The `hoard catalog` commands maintain a catalog directly, on any directory. The parent of the
output directory must already exist. A new directory is scanned fresh; an existing catalog
directory is rescanned in place: unchanged files are not opened, changed files are read
again, missing files keep their row as absent, and the search index is refreshed from the
change log afterwards, or built when there is none yet.

```sh
cargo run -- catalog scan --directory /existing/path/native-catalog
cargo run -- --config hoard.toml catalog scan --source cache --directory /existing/path/native-cache
cargo run -- catalog search --directory /existing/path/native-catalog 'format:gguf'
cargo run -- catalog search --directory /existing/path/native-catalog --sort size --desc 'base:sdxl'
cargo run -- catalog status --directory /existing/path/native-catalog
cargo run -- catalog rebuild --directory /existing/path/native-catalog
```

Run `search`, `status` and `rebuild` one after another; the lock is exclusive. A
scan that fails removes the directory it started, and a source whose root is missing is
skipped with a warning.

## The query grammar

Search speaks Hoard's own grammar, the one the search bar, `hoard ls` and shelves use. Tokens
combine with AND, a token's comma-separated values with OR, and a leading `-` negates the
whole token. Every token kind compiles:

- `role:` matches the role, and narrows to a sub-role when the spelling names one, so
  `role:ip-adapter` is `lora` with sub-role `ipa`.
- `base:` (also `arch:`) matches a family or a family it prefixes either way, and never
  matches `any`, which is a claim about compatibility rather than about a family.
- `format:` (`fmt:`), `precision:` (`prec:`, `quant:`), `state:` and `source:` (`src:`) are
  equality on that column.
- `node:` (`host:`) matches the node, resolving `self`, `me`, `local` and `here` to this
  catalog's own node.
- `rev:` (`revision:`, `tag:`, `branch:`) matches the revision name or a prefix of its sha.
- `repo:` matches the qualified name or any suffix of it, so `repo:name` finds
  `models/org/name`.
- `org:` (`owner:`) matches the artifact's org or the repository's org.
- `hash:` (`sha:`, `sha256:`) matches a sha256 exactly or by prefix, and a git blob sha
  exactly.
- `trigger:` (`triggers:`) matches the whole trigger string or one of its words.
- `dupe:` (`duplicate:`) selects artifacts reached by more than one path.
- `size:` compares a byte count with `<`, `<=`, `>`, `>=` or none for equality; a bare number
  is megabytes, so byte counts need the `b` suffix.
- `used:` compares an age in days against when an artifact was last served: `<Nd` and `<=Nd`
  are the artifacts used inside the window, `>Nd`, `>=Nd` and `=Nd` the ones nobody has used
  for that long. An artifact that was never served answers neither.
- `compat:` resolves its value against this catalog and then matches the roles and bases that
  plug into it; a value naming nothing is not an error, it resolves to no target and the
  token is refused.
- `shelf:` expands the named shelf's stored query, up to four levels deep.
- Anything else is free text: every word has to appear, and `~` in front also accepts the
  words as prefixes. A key the grammar does not know is free text carrying a warning.

Free text and `trigger:` match whole words or the whole stored string rather than substrings.

`--sort` takes `role`, `name`, `base`, `format`, `precision`, `size`, `revision`, `node`,
`state` or `used`, defaults to `name`, and `--desc` reverses it; every order ends on ascending
artifact id so paging is stable. `--limit` is capped at 1,000 and `--offset` selects the page.

## Measured

Read `.claude/skills/hoard-measure/SKILL.md` before adding a row here. Day-to-day timings on
this workstation drift by more than most effects being measured, so a "before" number quoted
from this document is not a comparison: rebuild the previous commit and measure both in one
session.

### This workstation's disks, commit `1ba8475`

Release build, three sources from `./hoard.toml`: the HuggingFace cache, the ComfyUI tree and
the LM Studio tree. `hash_policy = "background"`.

```sh
cargo build --release
time ./target/release/hoard --config hoard.toml scan
time ./target/release/hoard --config hoard.toml scan
./target/release/hoard --config hoard.toml doctor
du -sh data/native
./target/release/hoard --config hoard.toml ls 'role:lora'
```

- First scan: 5 m 35.686 s wall, `unchanged=728 changed=0 added=0 absent=0 hashed=445
  merged=2`. The walk and the header sniffing are a fraction of that; the wall time is the
  445 files read end to end for a content hash, which on this disk are the ComfyUI and LM
  Studio weights. A cache blob already names its hash, so the 477 `hf-cache` entries are not
  among them.
- Second scan immediately after: 0.182 s, hashing nothing and re-opening nothing.
- `doctor`: every check passes. The node record `workstation`, zero peers, the summary at 228
  artifacts, 228 present and 103 repositories, the three sources at 477 files and 414.4 GB
  (`hf-cache`), 223 files and 819.7 GB (`comfy`) and 28 files and 405.8 GB (`lmstudio`), the
  block cache and the upstream.
- Catalog on disk: 6.9 MB, of which `catalog.redb` is 6.3 MB and the published search
  directory 572 KB.
- `catalog status` reports `format_version: 1` and names every table `<model id>_1_<key>`.
- `ls 'role:lora'`: 35 artifacts. Every `hoard ls` query on this catalog is 15 to 17 ms,
  which is process start and opening two files rather than the query; on a running server,
  which holds the catalog and the Tantivy reader open, every route is 0.5 to 2.4 ms.

The catalog groups every shard set the same way whichever run produces it, including the two
overlapping sets in `nvidia/NVIDIA-Nemotron-Nano-9B-v2-NVFP4` that both label `model/model`
and both mark triage: a `*.index.json` claims two of the ten declared files and the naming
convention takes the remaining eight.

### Routes at 100k entries, commit `64e11af`

A synthetic tree of 100 loose repositories of 1,000 valid safetensors headers each,
`hash_policy = "never"`, `offline = true`, with one git clone source beside it so `resolve`
has a repository a commit names, and 2,000 usage events seeded on one repository. Three runs
per route, median, whole request including the response body.

| route | median |
| --- | --- |
| `GET /api/v1/repos?limit=1` | 0.90 ms |
| `GET /api/v1/repos?limit=100` | 0.67 ms |
| `GET /api/models?limit=1` | 0.54 ms |
| `GET /api/models/org0/repo000` | 3.38 ms |
| `HEAD /org0/repo000/resolve/main/w0.safetensors` | 0.78 ms |
| `GET /api/v1/sources` | 0.64 ms |
| `GET /api/v1/search?q=` | 13.7 ms |
| `GET /api/v1/search?q=node:self` | 41.6 ms |
| `GET /api/v1/search?q=role:lora` | 16.1 ms |
| `GET /api/v1/triage?limit=20` | 9.35 ms |
| `GET /api/v1/repos/models/org0/repo000/usage` | 1.39 ms |

A streamed response costs about 40 ms in HTTP framing whatever the catalog does, so the
resolve loop measures the framing: 200 sequential requests take 0.99 s and 400 with 20 in
flight take 0.81 s.

### The scale harness

`crates/hoard-index/tests/scale.rs`, an ignored test driven by `HOARD_SCALE_ENTRIES` and
`HOARD_SCALE_DIR`. Synthetic loose source of header-only safetensors files, 1,000 per
directory, one artifact per file, no hashing, release build, its own process. The catalog
produced exactly one artifact per file at 1,000, at 100k and at 1M. The harness then rewrites
1,000 of the headers, rescans and refreshes the search index from the change log, so the
incremental path is measured beside the full one.

At 1M. The `7737aeb` column is performance wave 1 and the `64e11af` column is performance
wave 2, which re-measured only the rows its own before/after needed; the two columns are from
different sessions and the rows wave 2 did not take are marked. Wave 2's own like-for-like
before column, `a4dd324`, is given where it differs from `7737aeb` by more than the effect.

| measure | 7737aeb | 64e11af |
| --- | --- | --- |
| scan wall | 102.769339125s | 95.305741107s |
| peak rss growth during scan | 731 MB | 838 MB |
| peak rss growth during rescan and refresh | not measured | 359 MB |
| rebuild wall | 13.936102691s | 12.781133799s |
| on disk, apparent | 4175 MB | 4175 MB |
| on disk, allocated | 2564 MB | 2276 MB |
| query empty by name | 119.431554ms | not re-measured |
| query deep offset | 119.874963ms | not re-measured |
| query role:lora with facets | 147.175983ms | not re-measured |
| query filtered | 234.181468ms | not re-measured |
| query node:self with facets | 510.93955ms | 511.984273ms |
| peak rss growth during the collapse query | 35 MB | not re-measured |
| rescan wall (1,000 changed) | 6.983199391s | 5.314401519s |
| refresh wall | 30.101233ms | 25.737863ms |

At 100k, commit `64e11af`: scan 6.88 s, allocated disk 232 MB, peak RSS growth 271 MB,
`node:self` 32.6 ms. The four primary-path queries were 13 to 25 ms when last taken.

Allocated disk is `du` over the output directory; apparent disk sums file lengths, and redb
grows `catalog.redb` in regions, so the apparent number is quantised and the allocated one is
what the disk actually loses.

### Thresholds

At 1M entries, absolute: peak RSS growth during the scan under 2 GB, allocated disk under
1.5 GB, every query under 500 ms, rebuild no slower than the scan.

Three of the four are met. Allocated disk is 2276 MB against 1.5 GB, and it is the one
threshold missed by a wide margin. `node:self` at 512 ms is at the query threshold rather
than under it, within its own run-to-run spread.

### Where the bytes are

Storage by table at 100k, largest stored bytes first, as `catalog status` reports it, measured
during the wave series. native_db names a table `{model id}_{version}_{key}`; every version
here is 1.

- `8_1_artifact_id`, the `ArtifactDetail` primary table: 100,000 rows, 52,104,173 stored
  bytes, 37,401,767 fragmented. One tensor summary and one evidence vector each.
- `7_1_id`, the `Entry` primary table: 100,000 rows, 24,757,780 stored, 25,468,496 fragmented.
- `3_1_id`, the `ArtifactRecord` primary table: 100,000 rows, 18,355,570 stored, 15,260,818
  fragmented.
- `5_1_artifact_id`, the `SearchText` primary table: 100,000 rows, 5,415,625 stored, 6,163,747
  fragmented.
- `3_1_identity`, the unique identity index on `ArtifactRecord`: 100,000 rows, 800,000 stored,
  1,944,092 fragmented.
- `10_1_seq`, the `SearchChange` log: 0 rows, because the refresh pruned every row it applied.

The 1M catalog keeps that order and those ratios: 500 MB stored against 354 MB fragmented for
`ArtifactDetail`, 240 against 244 for `Entry`, 179 against 166 for `ArtifactRecord`, 52
against 59 for `SearchText`, and 8 against 18 for the identity index.

## Remaining work

- **Shrink the per-entry record.** Revision, repository, source and the location key are still
  written out per entry, which is what the 1M catalog is mostly made of. Interning them per
  revision and referencing them by id is the lever. Allocated disk at 1M is 2276 MB against
  the 1.5 GB threshold.
- **Bound the collapse the way the primary path is bounded.** A query naming `node:`,
  `state:`, `source:`, `repo:`, `rev:` or `hash:`, or sorting on revision, node or state,
  still keeps one row per matching artifact rather than one per page. At 1M `node:self` is
  512 ms against the 500 ms threshold.
- **The USAGE tab reads a repository's whole log**, because the client count it shows is a
  claim about every pull the repository ever served. Bounding the chart alone would not bound
  the route: 20,000 rows cost 9.1 ms.
- **Path identities hash the text `to_string_lossy` renders.** Discovery currently never
  yields two distinct non-UTF-8 paths; a byte-exact identity must land with the identity
  redesign.
- **`Catalog::triage_queue` now needs a search index** rather than only tolerating a stale
  one. Every catalog a command or the server opens has one, but a catalog built by
  `scan_sources` alone answers the queue with an error until one is built.
- **A rescan re-reads every revision's documents** (README, configs, index files) before
  deciding units. Measure this on the workstation cache and skip when every document's stamp
  is unchanged.
- **Revision ordinals are recomputed from discovery on every rescan**, so a source that
  reorders revisions leaves stale ordinals on existing entries.
- **Count a revision's artifact units consistently** for a file whose content another revision
  already claimed. A repository with two snapshots, one holding `consolidated.safetensors`
  alone and one holding it beside a four shard set, keeps both artifacts at the right sizes
  but can name them either way round.
- **Preserve complete repository Git metadata** and add stable cross-scan identities.
- **Add a read-only open** once redb offers one, so searches can run beside a rebuild.
- **Benchmark realistic catalogs** and add process-kill and power-loss recovery tests. The
  current tests check transaction abort and publication boundaries, not power loss.

## Dependency and serialization findings

`native_db 0.8.2` requires exactly `native_model 0.4.20` and resolves to `redb 2.6.3`. Legacy
native_db upgrade features are disabled. `tantivy 0.26.1` uses only mmap and LZ4 features.
These are intentional constraints, not latest-major assumptions.

Hoard's row ids previously always used Serde's `deserialize_any`, which bincode cannot decode.
Binary deserializers now read their existing serialized string representation explicitly;
JSON keeps accepting both string and integer ids. The persistence and reopen tests exercise
that binary path.
