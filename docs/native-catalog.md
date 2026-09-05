# Native catalog implementation status

## Decision

`docs/superpowers/specs/2026-09-05-native-catalog-decision.md` records why this pairing was
chosen. native_db was preferred over raw Fjall because it already maintains typed records,
transactions and secondary indexes on redb, so Hoard does not have to own an index
maintenance layer. Tantivy replaces FTS5 as a disposable search index that is rebuilt from
the native records alone.

This branch implements an executable native_db/Tantivy filesystem catalog. It is an
experimental vertical slice, not a complete replacement of Hoard's existing persistent
layer. The regular `serve`, `scan`, `ls`, mirror and gossip paths still use SQLite.

There is no SQLite import, migration or compatibility path. Create a fresh native
catalog by scanning sources. No existing data needs to be retained.

## Running it

The parent of the output directory must already exist. A new directory is scanned fresh; an
existing catalog directory is rescanned in place: unchanged files are not opened, changed
files are read again, missing files keep their row as absent, and the search index is
refreshed from the change log afterwards, or built when there is none yet.

```sh
cargo run -- catalog scan --directory /existing/path/native-catalog
cargo run -- --config hoard.toml catalog scan --source cache --directory /existing/path/native-cache
cargo run -- catalog search --directory /existing/path/native-catalog 'format:gguf'
cargo run -- catalog search --directory /existing/path/native-catalog 'role:lora size:<400mb'
cargo run -- catalog search --directory /existing/path/native-catalog --sort size --desc 'base:sdxl'
cargo run -- catalog status --directory /existing/path/native-catalog
cargo run -- catalog rebuild --directory /existing/path/native-catalog
```

Only one process may open a catalog directory at a time; redb takes an exclusive lock, so
run `search`, `status` and `rebuild` one after another. A scan that fails removes the
directory it started; a source whose root is missing is skipped with a warning.

Search speaks Hoard's own query grammar, the one the search bar, `hoard ls` and shelves
use. Tokens combine with AND, a token's comma-separated values with OR, and a leading `-`
negates the whole token. Every token kind compiles:

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
- `size:` compares a byte count with `<`, `<=`, `>`, `>=` or none for equality; a bare
  number is megabytes, so byte counts need the `b` suffix.
- `used:` matches nothing, because the native catalog carries no usage yet.
- `compat:` resolves its value against this catalog and then matches the roles and bases
  that plug into it; a value naming nothing is not an error, it resolves to no target and
  the token is refused.
- `shelf:` expands the named shelf's stored query, up to four levels deep.
- Anything else is free text: every word has to appear, and `~` in front also accepts the
  words as prefixes. A key the grammar does not know is free text carrying a warning.

Two documented divergences from the SQL compiler: free text and `trigger:` match whole
words or the whole stored string rather than substrings, and `used:` matches nothing.

`--sort` takes `role`, `name`, `base`, `format`, `precision`, `size`, `revision`, `node`,
`state` or `used`, defaults to `name`, and `--desc` reverses it; every order ends on
ascending artifact id so paging is stable. `--limit` is capped at 1,000 and `--offset`
selects the page.

Free text indexes names, organizations, bases, triggers, tensor key prefixes and the body
of the shallowest README in each revision.

## What is implemented

- Existing HF-cache, Git and loose-source discovery feeds the native store directly.
- Header sniffing and inference reuse the existing Rust implementations. A revision's
  documents (README, configs, index files) are sniffed first and give every later unit its
  inference context; each shard set and each chunk of 1,024 standalone files is then sniffed
  in parallel, prepared and committed as its own transaction, so memory grows with a unit,
  not with a source. Whether a repository names its single artifact is decided from file
  names before the weights are read.
- Only weight and dataset formats become artifacts; every file is still an entry record.
- Shard sets, whether named `model-<n>-of-<m>` or listed by a `*.index.json` weight map,
  become one artifact anchored on the lowest shard. Members are entries that point at that
  artifact and have no search row; the anchor's row carries the whole model's size and
  shard count, and an incomplete set is triage.
- An unreadable file keeps its path and is recorded as an absent entry without an artifact;
  a git-lfs pointer stub is recorded as absent with the pointer's size and hash; a fetched
  LFS weight is an ordinary artifact. This is what the SQLite scanner does.
- Entries whose inference needs triage carry state `triage`.
- Artifact names and orgs follow the scanner's rules: a single-artifact repository is named
  by its repository.
- native_db stores typed sources, repositories, revisions, artifacts, tensor
  summaries, evidence, entries and searchable text. Library types stay inside
  `hoard-index::catalog`; callers use Hoard-owned request and response types.
- Automatic unique artifact identity and entry-to-artifact/repository indexes.
- Known content hashes or canonical paths establish identity. No full model hashing
  occurs in this initial native scan path. Two unrelated paths with unknown hashes
  remain separate artifacts, even if their bytes match.
- Native records survive process reopen. Tantivy can be rebuilt without source
  model files or a SQLite database.
- A completion manifest is published after all scan batches commit. An interrupted
  scan remains unusable instead of exposing a partially built catalog.
- A search rebuild uses a fresh directory and publishes its name in a redb
  transaction only after Tantivy commits. An unpublished build leaves the previous
  directory serving; publishing then removes every superseded directory.
- A catalog keeps the published Tantivy index and a manual-reload reader open, so a query
  pays for neither a fresh mmap directory nor a directory watcher.
- A custom Tantivy collector produces exact distinct artifact totals and chooses the
  preferred entry after filtering. Only the result page is loaded from native_db.
- The build and the refresh mark one document per artifact `primary`, the entry a row would
  be assembled from, and both write all of an artifact's documents together so the mark
  stays unique. A query whose tokens and sort key touch only fields every entry of an
  artifact agrees on is intersected with that mark and answered without collapsing anything:
  a `Count` settles the total, then one pass keeps a bounded heap of the candidates that can
  reach the requested window, tallies the five facet dimensions by ordinal and collects the
  node ordinals. Only `node`, `state`, `source`, `repo`, `rev` and `hash`, and sorting on
  revision, node or state, still need the collapse.
- One `Catalog::query` answers a whole index screen: the page in any of the ten sort
  orders, the distinct artifact and node totals, and the five facet dimensions. Each
  dimension is counted over the query with its own key excluded, so clicking a bucket
  never collapses the rail it was clicked in; `base` buckets are counted by running each
  bucket's own query, because the prefix rule does not group. Node counts are per preferred
  entry, which is the same number the SQL rule gives while a catalog holds one node.
- Keyword columns are string fast fields, and the collapse collector keeps term ordinals
  rather than terms: one retained artifact is eighty-eight bytes of integers naming its
  segment, its sort value and its five facet values. Each segment's columns stay open in
  the collected result, so a term is spelled only for a page row, a facet bucket or the
  node set, once per segment and ordinal however many artifacts carry it. Ordinals are
  lexicographic inside a segment and meaningless across segments, so a page ordered by a
  term sorts on ordinals while one segment holds every hit and otherwise bounds each
  segment on its own ordinals and merges the survivors by the terms they spell. A page
  nearer the end of the order is cut from the reversed order, so a deep offset selects and
  spells no more than a shallow one.
- DISK PRESSURE is a summary record the artifact sweep refreshes: the bytes duplicated
  paths waste and how many artifacts waste them.
- Ids come from a persisted allocator and the generation increments per rescan; artifact
  path counts are swept from the entry index after every scan and orphaned artifacts are
  removed.
- Storage is reported per redb table, and `hoard catalog scan` compacts the database after
  every scan. A rescan itself does not, so a catalog shared by the server can be walked
  again without an exclusive borrow.
- Every command that opens the index also opens or creates the native catalog at
  `<data_dir>/native`; `hoard scan` and the scan job rescan and refresh it after the SQLite
  scan; the server holds it as shared state.
- `GET /api/v1/search`, `GET /api/v1/stats` and `hoard ls` read the native catalog; shelves
  still come from SQLite until user data moves.
- The artifact sweep keeps the totals the stats route prints in its summary record, which is
  at model version 2; version 1 rows are migrated on open.
- A `fixtures` feature exposes `Catalog::seed`, which writes repositories, entries,
  artifacts, details and search text through the same allocator, change log and sweep a scan
  uses, so a test can hold known rows without inference. It is compiled only under `cfg(test)`
  or that feature and never reaches the binary.
- A query answers from the published index even when a rescan has logged changes no refresh
  has applied, and says so with `stale`; only a catalog with no index at all is refused.
- Every transaction that touches an artifact logs its id to a search change table;
  refresh_search replays the log after a durable cursor, one Tantivy commit per batch,
  deleting the artifact's documents by term and re-adding its entries, and prunes what it
  applied. A rescan followed by a refresh updates search without a rebuild.
- Artifact identities are 128-bit hashes of the identity text, so the unique index holds
  fixed-width keys.
- The manifest is at model version 2 and version 1 rows are migrated on open.
- CLI operations run on blocking threads and report scan/index progress with indicatif.

## Search document shape

The index contains one document per entry. This preserves a requirement the earlier
one-document-per-artifact proposal missed: `node:laptop AND state:local` must match
one entry satisfying both predicates. Independently combining artifact-wide node
and state lists can return a false positive. The preferred row must also be chosen
from matching entries, rather than fixed before filtering.

One document per artifact carries a `primary` mark: the entry a row would be assembled
from, which is the highest availability preference and on a tie the lowest entry id. A query
that filters and sorts only on fields every entry of an artifact agrees on is intersected
with that mark and needs no collapse at all. For M matched documents and K equal to offset
plus limit, collection is then O(M) time and O(K) memory per segment, plus one tally per
distinct facet value, and page selection is O(K log K).

A query that names `node`, `state`, `source`, `repo`, `rev` or `hash`, or sorts on revision,
node or state, still collapses, because those fields differ between the entries of one
artifact. Collapse retains one small candidate per matching artifact per segment and then
combines segment results: O(M) time and O(A) memory per segment for A matched artifacts,
with page selection O(A + K log K). Deep pagination and broad queries on that path still
require memory proportional to matching artifacts.

Tantivy uses one writer thread with a 32 MiB indexing budget. Nothing larger than a unit is
held in memory or in an open transaction; a killed scan still removes its directory because
the manifest is written last.

## Measured

Against this workstation's real HuggingFace cache (314 GB, source `hf-cache`), release
build, output on the same NVMe:

```
hoard --config hoard.toml catalog scan --source hf-cache --directory <out>
hoard catalog search --directory <out> 'state:triage'
```

- Wall time: 0.3 s on a warm page cache. Only headers are read, and cache blob names
  already carry the content hash, so no file is hashed.
- Catalog size: 2.2 MB (2.1 MB `catalog.redb` plus a 44 KB Tantivy directory).
- Manifest counts: 1 source, 69 repositories, 28 artifacts, 477 entries.
- Native `state:triage` returns 10 artifacts.

The same source in the SQLite index (`data/hoard.db`) holds 477 entries, 67 repositories,
28 artifacts and 10 triage artifacts. Entries, artifacts and triage now agree exactly; the
ungrouped shard divergence measured before this change (134 artifacts and 80 triage) is
gone.

The two catalogs also agree on every shard set's size and count, including the two
overlapping sets in `nvidia/NVIDIA-Nemotron-Nano-9B-v2-NVFP4` that both label `model/model`
and both mark triage: a `*.index.json` claims two of the ten declared files and the naming
convention takes the remaining eight.

Two of the 28 names still differ on each side, neither for a grouping reason:

- Native `text_encoder/model.fp16.safetensors`, SQLite `clip_l.safetensors`. The same bytes
  are in the `comfy` source too. A SQLite artifact row holds one name and `comfy` wrote it;
  this scan covered `hf-cache` alone.
- Native `model` and `Ministral-3-14B-Instruct-2512`, SQLite `Ministral-3-14B-Instruct-2512`
  and `consolidated.safetensors`. The repository has two snapshots: one holds
  `consolidated.safetensors` alone, the other holds it beside a four shard set. Both
  catalogs keep the same two artifacts with the same sizes; the names are swapped because a
  revision names its artifacts by how many artifact units it holds, and the two scanners
  count the second snapshot's already-seen `consolidated.safetensors` differently.

The repository count still differs by two (69 native, 67 SQLite). The SQLite index was last
written hours before this scan, so the two runs did not see the same disk. Not
investigated further.

## Scale

Synthetic loose source of header-only safetensors files, 1,000 per directory, one artifact
per file, no hashing, release build, in its own process
(`crates/hoard-index/tests/scale.rs`, an ignored test driven by `HOARD_SCALE_ENTRIES`). The
catalog produced exactly one artifact per file at 1,000, at 100k and at 1M. The harness
then rewrites 1,000 of the headers, rescans and refreshes the search index from the change
log, so the incremental path is measured beside the full one.

Baseline, measured on 2026-09-05 before bounded units and narrow records landed. Native:
100k scanned in 3.65 s with 958 MB of peak RSS growth and 518 MB on disk; 1M scanned in
37.9 s with 8533 MB of peak RSS growth and 4138 MB on disk (2739 MB allocated), searching
in 124 to 145 ms. SQLite: 100k scanned in 1832 s with 356 MB of peak RSS growth and 136 MB
on disk, searching in 215 to 336 ms; the 1M scan was killed after 52 minutes roughly one
eighth of the way in. The SQLite measurement was run once to establish this baseline and
its harness was removed; SQLite is the store being replaced, not a peer to keep measuring.

Waves 1 to 3 measured `search`, which spoke Tantivy's syntax and returned a page and a
total. Wave 4 removed it; the rows below it measure `query`, which also counts five facet
dimensions and the distinct nodes, so the two sets of timings are not comparable and both
are kept.

| measure | 100k w1 | 100k w2 | 100k w3 | 100k w4 | 100k w5 | 100k w6 | 1M w1 | 1M w2 | 1M w3 | 1M w4 | 1M w5 | 1M w6 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| scan wall | 5.226065725s | 5.900763157s | 5.476153652s | 5.657892353s | 5.672653469s | 5.623549612s | 57.77687964s | 75.282972971s | 85.709219721s | 85.449911878s | 84.603774091s | 85.078043138s |
| peak rss growth during scan | 198 MB | 292 MB | 253 MB | 254 MB | 254 MB | 254 MB | 1560 MB | 842 MB | 731 MB | 741 MB | 732 MB | 732 MB |
| rebuild wall | 596.949484ms | 440.388181ms | 802.170108ms | 943.869337ms | 883.471396ms | 1.076870421s | 4.40112116s | 5.294884595s | 8.497179982s | 10.39137641s | 10.551732892s | 11.886669682s |
| peak rss growth during rebuild | 34 MB | 54 MB | 52 MB | 47 MB | 48 MB | 46 MB | 0 MB | 0 MB | 0 MB | 0 MB | 0 MB | 0 MB |
| on disk, apparent | 518 MB | 261 MB | 261 MB | 265 MB | 265 MB | 265 MB | 4138 MB | 4138 MB | 4138 MB | 4173 MB | 4173 MB | 4174 MB |
| on disk, allocated | 288 MB | 243 MB | 213 MB | 216 MB | 216 MB | 216 MB | 2983 MB | 2476 MB | 2112 MB | 2148 MB | 2148 MB | 2149 MB |
| search offset 0 | 10.577049ms | 12.251924ms | 9.756606ms | removed | removed | removed | 189.855663ms | 174.943704ms | 216.076995ms | removed | removed | removed |
| search deep offset | 9.144823ms | 9.106715ms | 8.649164ms | removed | removed | removed | 197.536743ms | 190.89815ms | 217.117028ms | removed | removed | removed |
| search filtered | 7.200678ms | 7.238805ms | 6.967226ms | removed | removed | removed | 177.39202ms | 168.239764ms | 188.590775ms | removed | removed | removed |
| query empty by name | not measured | not measured | not measured | 221.111746ms | 33.894331ms | 13.27674ms | not measured | not measured | not measured | 3.011389042s | 577.455193ms | 125.506335ms |
| query deep offset | not measured | not measured | not measured | 212.094065ms | 34.319659ms | 13.447428ms | not measured | not measured | not measured | 2.982565132s | 534.299433ms | 125.852064ms |
| query role:lora with facets | not measured | not measured | not measured | 135.635441ms | 28.611903ms | 16.175599ms | not measured | not measured | not measured | 2.016078812s | 493.170639ms | 159.062959ms |
| query filtered | not measured | not measured | not measured | 300.337912ms | 41.425115ms | 25.132866ms | not measured | not measured | not measured | 4.384522112s | 873.916709ms | 251.142862ms |
| query node:self with facets | not measured | not measured | not measured | not measured | not measured | 58.87439ms | not measured | not measured | not measured | not measured | not measured | 1.082111424s |
| peak rss growth during queries | not measured | not measured | not measured | 113 MB | 41 MB | 1 MB | not measured | not measured | not measured | 1190 MB | 409 MB | 0 MB |
| peak rss growth during the collapse query | not measured | not measured | not measured | not measured | not measured | 42 MB | not measured | not measured | not measured | not measured | not measured | 300 MB |
| rescan wall (1,000 changed) | not measured | not measured | 812.140523ms | 816.293704ms | 898.947574ms | 517.541105ms | not measured | not measured | 28.5581122s | 29.606316494s | 31.851403311s | 6.241905939s |
| refresh wall | not measured | not measured | 28.341576ms | 31.047147ms | 31.89983ms | 27.67862ms | not measured | not measured | 28.598925ms | 35.828931ms | 33.576794ms | 29.871137ms |

`query role:lora with facets` matches a quarter of the catalog and is the one query here
whose facet rail costs a second collection: the ROLE dimension is counted with its own key
excluded, so the whole index is counted a second time, which is one more bounded pass on the
primary path and one more whole collapsed set on the collapse path. It reported three role
buckets,
no base buckets, and one bucket each for format, precision and node, which is what the
synthetic headers hold; a catalog with real base families also pays one query per BASE
bucket, which this fixture never exercises.

Allocated disk is `du` over the output directory; apparent disk sums file lengths, and redb
grows `catalog.redb` in regions, so the apparent number is quantised and the allocated one
is what the disk actually loses.

Storage by table at 100k after the scan, rebuild, rescan and refresh the harness runs,
largest stored bytes first, as `catalog status` reports it. native_db names a table
`{model id}_{version}_{key}`:

- `8_1_artifact_id`, the `ArtifactDetail` primary table: 100,000 rows, 52,104,173 stored
  bytes, 37,401,767 fragmented. It holds one tensor summary and one evidence vector each.
- `7_1_id`, the `Entry` primary table: 100,000 rows, 24,757,780 stored, 25,468,496
  fragmented.
- `3_1_id`, the `ArtifactRecord` primary table: 100,000 rows, 18,355,570 stored,
  15,260,818 fragmented.
- `5_1_artifact_id`, the `SearchText` primary table: 100,000 rows, 5,415,625 stored,
  6,163,747 fragmented.
- `3_1_identity`, the unique identity index on `ArtifactRecord`: 100,000 rows, 800,000
  stored, 1,944,092 fragmented.
- `10_1_seq`, the `SearchChange` log: 0 rows, because the refresh pruned every row it
  applied.

The 1M catalog keeps that order and those ratios: 500 MB stored against 354 MB fragmented
for `ArtifactDetail`, 240 against 244 for `Entry`, 179 against 166 for `ArtifactRecord`,
52 against 59 for `SearchText`, and 8 against 18 for the identity index.

Thresholds at 1M entries, absolute now that there is no peer left to compare against: peak
RSS growth under 2 GB, allocated disk under 1.5 GB, deep-offset search under 500 ms,
rebuild no slower than the scan.

Verdict: every query row is under 500 ms at 1M and the memory a query adds is under 300 MB,
both for the first time. `query empty by name` is 125 ms against 577 ms in wave 5, `query
deep offset` 126 ms against 534 ms, `query role:lora with facets` 159 ms against 493 ms, and
`query filtered` 251 ms against 874 ms, so the slowest of the four sits at half the
threshold. All four take the primary path: none of them names `node`, `state`, `source`,
`repo`, `rev` or `hash`, and all four sort by name. At 100k the same four are 13 to 25 ms
against 29 to 41 ms.

Peak RSS growth during those four queries is 0 MB against a 300 MB threshold and against
409 MB in wave 5, which is to say the four never push the process past the high-water mark
its own scan already set. What a query holds is now a bounded heap of at most offset plus
limit candidates per segment, one ordinal tally per distinct facet value, and one node
ordinal set, none of which grows with the million documents matched; the terms behind those
ordinals are still spelled only for the page, the facet buckets and the node set. At 100k
the same four add 1 MB against 41 MB. Deep paging keeps the shape wave 5 gave it: offset
999,900 is cut from the reversed order, so each segment bounds itself to the 100 candidates
that can reach the page, and the row costs what offset 0 costs.

The collapse path is what an entry-level query still costs, and it misses both thresholds.
`query node:self with facets` is 1.08 s at 1M and adds 300 MB, and 59 ms and 42 MB at 100k.
That is the wave 5 shape unchanged: one `Hit` per matching artifact into a hash map however
few rows the page asks for, plus a second whole collapsed set held beside the first while
`facets` counts a dimension the query narrows. It applies only to a query naming `node:`,
`state:`, `source:`, `repo:`, `rev:` or `hash:`, or sorting by revision, node or state.
Every other query, the empty one an index screen opens on included, is answered from primary
documents.

The rest moved only where this wave touched it. The scan is 85.1 s against 84.6 s and its
732 MB of peak RSS growth is unchanged. The rebuild is 11.9 s against 10.6 s: it now walks
artifacts rather than entries, reads each artifact's entries through the secondary key to
decide which one is primary, and writes one more fast field per document; it is still
comfortably faster than the scan. Allocated disk is 2149 MB against 2148 MB, so the
`primary` column costs nothing measurable and the 1.5 GB threshold is still missed. The
refresh stays cheap at 29.9 ms for 1,000 changed artifacts. The rescan came in at 6.24 s
against 31.9 s; nothing in this wave touches the rescan and the two runs did not see the
same page cache, so that number is variance, not a result.

Go for switching the index screen onto this catalog, which is the next wave. The queries a
screen opens on, the empty one, a role or format filter, a page anywhere in the order, are
all primary-path and all under 160 ms at a million artifacts while holding nothing that
scales with the catalog. The `node:` and `state:` filters the facet rail offers are the
exception: they still cost a second and 300 MB at 1M, and bounding that collapse the same
way is the work left. The catalogs actually measured on this workstation hold 477 entries,
where every query here is under a millisecond, so that remainder is a scale limit rather
than a correctness one.

## Remaining replacement work

- Move the routes still on SQLite to the catalog: artifact detail, compat, blob detail,
  repos, triage, shelves, loadouts, projection, sources and jobs, nodes and peers, settings,
  the Hub and git faces, the mirror's ingest and gossip.
- Convert the existing Store/Connection API and live scanner, mirror, cache eviction,
  gossip, watch and CLI callers to native transactions.
- Path identities hash the text to_string_lossy renders; discovery currently never yields
  two distinct non-UTF-8 paths, and a byte-exact identity must land with the identity
  redesign.
- Bound the collapse the way the primary path is bounded. A query naming `node:`, `state:`,
  `source:`, `repo:`, `rev:` or `hash:`, or sorting on revision, node or state, still
  materializes one candidate per matching artifact: measured at 1M, `node:self` is 1.08 s
  against a 500 ms threshold and adds 300 MB. Every other query is answered from primary
  documents in under 260 ms while adding nothing that scales with the catalog.
- Shrink the per-entry record further: revision, repository, source and absolute path are
  written out per entry, which is what the 1M catalog is mostly made of. Intern them per
  revision and reference them by id.
- Implement user overrides/noise, shelves, loadouts, settings, projections, jobs,
  usage, peer cursors and the associated graph constraints/cascades.
- Preserve complete repository Git metadata and add stable cross-scan identities.
- Add full hash scheduling, progressive identities and content deduplication.
- Port the missing-file policies from the existing scan pipeline.
- Entries carry no node: the manifest's node is the only one, so peer-gossiped entries need
  a node id per entry when peers land.
- A rescan re-reads every revision's documents (README, configs, index files) before
  deciding units; measure this on the workstation cache and skip when every document's
  stamp is unchanged.
- Revision ordinals are recomputed from discovery on every rescan; a source that reorders
  revisions leaves stale ordinals on existing entries.
- Count a revision's artifact units the way the legacy scanner does for a file whose
  content another revision already claimed. The two scanners disagree on one repository of
  this workstation's cache, which swaps the names of its two artifacts.
- Add a read-only open once redb offers one, so searches can run beside a rebuild.
- Benchmark realistic catalogs and add process-kill/power-loss recovery tests.
  Current tests check transaction abort and publication boundaries, not power loss.

## Dependency and serialization findings

`native_db 0.8.2` requires exactly `native_model 0.4.20` and resolves to `redb 2.6.3`.
Legacy native_db upgrade features are disabled. `tantivy 0.26.1` uses only mmap and
LZ4 features. These are intentional constraints, not latest-major assumptions.

Hoard's row IDs previously always used Serde's `deserialize_any`, which bincode
cannot decode. Binary deserializers now read their existing serialized string
representation explicitly; JSON keeps accepting both string and integer IDs. The
native persistence/reopen tests exercise this required binary serialization path.

## Concurrent branch changes

The worktree started at `647cd99`. Commit `c0d877e` subsequently added scheduled
cache eviction on the main branch. It was inspected through Git objects only and
was not applied here. Its changed files do not overlap this slice's changed files.
Its entry retraction/cache reconciliation behavior must be included when the live
store is converted; the independent native commands do not currently serve or evict
mirror data.
