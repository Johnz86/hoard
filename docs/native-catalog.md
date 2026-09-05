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
rebuilt afterwards.

```sh
cargo run -- catalog scan --directory /existing/path/native-catalog
cargo run -- --config hoard.toml catalog scan --source cache --directory /existing/path/native-cache
cargo run -- catalog search --directory /existing/path/native-catalog 'format:gguf'
cargo run -- catalog search --directory /existing/path/native-catalog 'role:lora AND size:[0 TO 400000000]'
cargo run -- catalog status --directory /existing/path/native-catalog
cargo run -- catalog rebuild --directory /existing/path/native-catalog
```

Only one process may open a catalog directory at a time; redb takes an exclusive lock, so
run `search`, `status` and `rebuild` one after another. A scan that fails removes the
directory it started; a source whose root is missing is skipped with a warning.

Search uses Tantivy query syntax. Bare words are ANDed; quoted phrases, `AND`, `OR`,
`NOT`, exact field filters and numeric byte ranges are supported. It does not yet
compile Hoard's existing search grammar, shelves or compatibility expressions.
Keywords use exact spelling. Results use descending size and ascending artifact ID;
`--limit` is capped at 1,000 and `--offset` selects the page.

Exact fields: `role`, `sub_role`, `base`, `format`, `precision`, `node`, `state`,
`source`, `repo`, `revision`, `org`, `hash`, `trigger`. `size` is a numeric byte count.
Free text indexes names, organizations, bases, triggers, tensor key prefixes and
the body of the shallowest README in each revision. Trigger field filters currently match
the complete trigger string; they do not implement legacy substring matching.

## What is implemented

- Existing HF-cache, Git and loose-source discovery feeds the native store directly.
- Header sniffing and inference reuse the existing Rust implementations. A revision's
  documents (README, configs, index files) are sniffed first and give every later unit its
  inference context; each shard set and each chunk of 256 standalone files is then sniffed
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
- Ids come from a persisted allocator and the generation increments per rescan; artifact
  path counts are swept from the entry index after every scan and orphaned artifacts are
  removed.
- Storage is reported per redb table, and the database is compacted after every scan.
- CLI operations run on blocking threads and report scan/index progress with indicatif.

## Search document shape

The index contains one document per entry. This preserves a requirement the earlier
one-document-per-artifact proposal missed: `node:laptop AND state:local` must match
one entry satisfying both predicates. Independently combining artifact-wide node
and state lists can return a false positive. The preferred row must also be chosen
from matching entries, rather than fixed before filtering.

Artifact collapse retains one small candidate per matching artifact per segment,
then combines segment results. For M matched entries and A matched artifacts,
collection is expected O(M) time and O(A) memory per segment. Page selection is
O(A + K log K), where K is offset plus limit. Deep pagination and broad queries
still require memory proportional to matching artifacts. This needs measurement
before a large-scale adoption decision; no throughput claim is made here.

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
catalog produced exactly one artifact per file at 1,000, at 100k and at 1M.

Baseline, measured on 2026-09-05 before bounded units and narrow records landed. Native:
100k scanned in 3.65 s with 958 MB of peak RSS growth and 518 MB on disk; 1M scanned in
37.9 s with 8533 MB of peak RSS growth and 4138 MB on disk (2739 MB allocated), searching
in 124 to 145 ms. SQLite: 100k scanned in 1832 s with 356 MB of peak RSS growth and 136 MB
on disk, searching in 215 to 336 ms; the 1M scan was killed after 52 minutes roughly one
eighth of the way in. The SQLite measurement was run once to establish this baseline and
its harness was removed; SQLite is the store being replaced, not a peer to keep measuring.

| measure | native 100k | native 1M |
| --- | --- | --- |
| scan wall | 5.226065725s | 57.77687964s |
| peak rss growth during scan | 198 MB | 1560 MB |
| rebuild wall | 596.949484ms | 4.40112116s |
| peak rss growth during rebuild | 34 MB | 0 MB |
| on disk, apparent | 518 MB | 4138 MB |
| on disk, allocated | 288 MB | 2983 MB |
| search offset 0 | 10.577049ms | 189.855663ms |
| search deep offset | 9.144823ms | 197.536743ms |
| search filtered | 7.200678ms | 177.39202ms |

Thresholds at 1M entries, absolute now that there is no peer left to compare against: peak
RSS growth under 2 GB, allocated disk under 1.5 GB, deep-offset search under 500 ms,
rebuild no slower than the scan.

Verdict: peak RSS growth under 2 GB is met, 1560 MB at 1M and 198 MB at 100k, against
8533 MB and 958 MB before the revision was split into units; the scan now holds one shard
set or one chunk of 256 files at a time instead of a whole source, and pays 57.8 s for it
against 37.9 s. Deep-offset search under 500 ms is met, 197 ms at 1M. Rebuild no slower
than the scan is met, 4.4 s against 57.8 s. Allocated disk under 1.5 GB is missed, and
missed worse than before: 2983 MB at 1M against 2739 MB for the whole-source scan, 2941 MB
of it `catalog.redb` and the rest Tantivy. Two things ate the narrowing. The synthetic tree
is one artifact per file with no shard set and no shared path, so the artifact-wide fields
`SearchRow` used to copy per entry were never copied more than once here and dropping them
saved nothing this benchmark can show; against that, splitting the tensor summary and
evidence out of `ArtifactRecord` added a second per-artifact record, one million more keyed
rows, and the scan now commits about 3,900 transactions per million entries instead of one,
each writing a fresh path through every B-tree it touches. This is a go for continuing the
replacement: memory, search latency and rebuild time all clear their thresholds and the
disk miss is a storage-layout problem with named causes, to be settled by a per-entry
record that stops repeating its revision, repository and absolute path and by measuring a
tree with shared artifacts, not by anything redb or Tantivy does.

## Remaining replacement work

- Convert the existing Store/Connection API and live server, scanner, mirror, cache
  eviction, gossip, watch and CLI callers to native transactions.
- Path identities render through to_string_lossy; discovery currently never yields two
  distinct non-UTF-8 paths, and a byte-exact identity must land with the identity redesign.
- Bound the search collector: it materializes one candidate per matching artifact per
  segment, so the empty query costs memory proportional to the catalog on every call;
  replace it with a bounded heap keyed on (size, artifact id) plus an exact distinct count
  before wiring search into serve.
- Shrink the per-entry record further: revision, repository, source and absolute path are
  written out per entry, which is what the 1M catalog is mostly made of. Intern them per
  revision and reference them by id.
- Add a transactional search outbox with replay so a rescan updates Tantivy incrementally
  instead of rebuilding it.
- Implement user overrides/noise, shelves, loadouts, settings, projections, jobs,
  usage, peer cursors and the associated graph constraints/cascades.
- Preserve complete repository Git metadata and add stable cross-scan identities.
- Add full hash scheduling, progressive identities and content deduplication.
- Port the missing-file policies from the existing scan pipeline.
- Count a revision's artifact units the way the legacy scanner does for a file whose
  content another revision already claimed. The two scanners disagree on one repository of
  this workstation's cache, which swaps the names of its two artifacts.
- Add a read-only open once redb offers one, so searches can run beside a rebuild.
- Port the complete Hoard grammar, aliases, local-node resolution, exclusions,
  facets, compatibility, shelf expansion and every sorting mode.
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
