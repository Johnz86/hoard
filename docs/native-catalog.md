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
nothing here restates it.

## Layout

`<data_dir>/native/` holds:

- `catalog.redb`, the one file every record lives in.
- one published `search-v1-<nanos>/` directory holding the Tantivy index. A rebuild writes a
  new directory and publishes its name in a redb transaction only after Tantivy commits; an
  unpublished build leaves the previous one serving, and publishing removes every superseded
  directory.

The whole thing is recreatable from disk plus peers. redb takes an exclusive lock, so one
process may open a catalog at a time; `hoard doctor` reports it when a second one tries.

## How a record is encoded

native_model writes an eight byte header naming the record and its version, then a body in
whichever codec `#[native_model(with = ...)]` names. `catalog/codec.rs` holds two, and the
records that do not name one keep native_model's own bincode default, which is what
`Manifest` bootstraps from before any changed record is read.

- `Compact` is bincode 1.3 with variable length integers, little endian, trailing bytes
  refused. It is on `Entry` and `ArtifactRecord`. native_model's default spends eight bytes
  on every string and vector length, four on every enum discriminant and eight on every
  integer whatever it holds; nothing outside this process reads a row, so none of that is
  paid for.
- `Cold` wraps `Compact`'s bytes in a one byte tag and, when the whole envelope comes out
  smaller, an LZ4 block with a checked four byte decoded length. It is on `ArtifactDetail`
  and `SearchText`, the two largest tables and the two nothing scans: the inspector reads
  one row and a rebuild reads one per artifact. Each row is compressed on its own, so a read
  is still one primary lookup and one bounded decode, and a length claiming more than the
  256 MiB maximum is refused before anything is allocated for it.

Hashes and row ids serialize as bytes and integers to a binary format and as hex and
prefixed strings to a human-readable one, so the catalog is compact and the JSON the API
answers with is unchanged. `hoard-core`'s `id.rs` is where that branch lives.

`Entry` carries no location text. The unique index over where a row sits is computed from
the three fields that say so, as `repo_id` big endian, then `revision` big endian, then the
relative path's own bytes. The first 8 bytes are exactly one repository and the first 12
exactly one revision, so a repository listing and a revision listing are both one prefix
scan of that index and `Entry` needs no repository index of its own.

`hoard catalog` takes any directory with `--directory`. Every other command opens or creates
the catalog at `<data_dir>/native`.

## Modules

`crates/hoard-index/src/catalog/`:

- `mod.rs` open, `open_or_scan`, status, integrity, `FORMAT_VERSION`, the per-table storage
  report, compact.
- `codec.rs` the compact and cold record encodings, named by `#[native_model(with = ...)]`.
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
  the transaction that made the change, and writes nothing while no search directory is
  published. The sequence continues past both the highest key the log holds and
  `Manifest.search_seq`.
- `Counters::save(transaction)` flushes the shift and writes the allocator back.

**Every writer of an entry moves the derived counts.** A writer that skips `DerivedShift`
leaves `Repository::present_files`, `Repository::present_bytes`, `Source::pending_hashes` and
`Node::filed` behind; `hoard doctor`'s summary check is what catches it. A writer that skips
`log_changes` leaves the search index stale with no record that it is.

A pass that runs before any index is published writes no `SearchChange` rows at all. There is
nothing for them to be a difference against: the build that follows reads every artifact
record, and publishing prunes the whole log unread. `Counters::load` reads whether the
manifest names a published directory and `log_changes` returns early while it does not, so a
fresh scan of a million artifacts no longer writes and then deletes a million rows.

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
that mark and needs no collapse. That path selects its window one of two ways, and both
count every matched document once and tally it into every facet dimension once.

For M matched documents and K equal to offset plus limit, a window with K at most 16,384 for
a term key, or 131,072 for a numeric one, is answered by one pass that keeps a heap of the K
rows that can reach it: O(M) time and O(K) memory per segment, plus one tally slot per term
of each facet column's dictionary in each segment, and page selection O(K log K). Both
constants are measured, in "Thresholds" below; they differ because the heap of a term key
spells a term for every row it holds and a numeric key only compares integers.

A deeper window is found by rank instead, in three passes. The first counts and tallies the
match and keeps every S-th matched row's sort key, for a sample S of about the square root of
M, floored at 64 and capped at 4,096; the second counts the matched documents between
consecutive sampled keys; the third keeps the documents of the one bucket that holds the
requested rank, plus a heap of one page beyond it. That is O(M log S) time and
O(S + B + limit) memory for a bucket B capped at 65,536. The square root is where the two
costs meet: mapping a boundary into a segment costs a dictionary seek, so the second pass
starts at O(S x segments), and the bucket the third pass reads holds about M/S rows. The
stride is forced odd, because a stride sharing a factor with a period in the order documents
were written samples only some residues of that period and leaves whole runs of equal keys
with no boundary between them.

A bucket over the cap costs more to hold than the heap does, so the heap answers the request
instead and nothing about the answer depends on the sample. Either way, a window nearer the
far end is cut from the reversed order, so the deepest window either selection sees is half
the match.

A query naming `node`, `state`, `source`, `repo`, `rev` or `hash`, or sorting on revision,
node or state, still collapses, because those fields differ between the entries of one
artifact. Collapse retains one small candidate per matching artifact per segment and then
combines segment results: O(M) time and O(A) memory per segment for A matched artifacts, with
page selection O(A + K log K). Deep pagination and broad queries on that path still require
memory proportional to matching artifacts.

Keyword columns are string fast fields and the collectors keep **term ordinals**, not terms.
Ordinals are lexicographic inside a segment and meaningless across segments, so a page
ordered by a term sorts on ordinals while one segment holds every hit and otherwise bounds
each segment on its own ordinals and merges the survivors by the terms they spell. A sampled
boundary crosses segments the same way: its ordinal is spelled once, the terms settle the
order between boundaries, and each boundary is then mapped back into every segment as the
lowest ordinal whose term is not below it, with whether that ordinal spells the boundary's
own term rather than the next one above it. A term is spelled once per segment and ordinal,
only for a page row, a boundary, a facet bucket or the node set. A page nearer the end of the
order is cut from the reversed order, so a deep offset selects and spells no more than a
shallow one.

A tally is a **vector indexed by term ordinal**, one per facet dimension per segment, sized
at that column's `num_terms()`; the base dimension carries a second vector of the label
ordinal its artifacts agreed on. A segment ordinal is a constant inside a segment collector
and a facet dictionary is small, so counting a document is an increment at an index rather
than a hash of a `(segment, dimension, ordinal)` key. The node total reads the ordinals of
the node dimension that at least one document sat on. Segments are merged by the terms their
ordinals spell, once per segment and value.

Documents are read **a block at a time**. A collector that takes no score is driven through
`for_each_no_score`, which hands `collect_block` up to 64 documents at once, so each column
is asked once per block through `Column::first_vals` into a buffer the segment collector
reuses, rather than once per document per column. `collect` is one block of one document, so
there is one path. All five collectors read this way: the two that tally read the facet
ordinals and the base label as well as the row, the collapse and window collectors read the
row alone, and the bucket collector reads only the sort key and the artifact id.

Every collector overrides `Collector::collect_segment`, because tantivy's own
`default_collect_segment_impl` drops to one `collect` per document as soon as a segment has a
delete file, and every segment a refresh has touched has one. The alive filter therefore lives
here instead, filtering each block into a buffer the segment reuses, so a block read survives
deletes. Without it the block path would be dead on exactly the segments a long-lived index
accumulates, and slower than what it replaced.

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
cargo run -- catalog compact --directory /existing/path/native-catalog
```

Run `search`, `status`, `rebuild` and `compact` one after another; the lock is exclusive. A
scan that fails removes the directory it started, and a source whose root is missing is
skipped with a warning.

`compact` is the only thing that compacts a catalog on demand. A scan compacts at the end of
building one, because that is bulk construction; a rescan never does, so a shared catalog
stays readable. `compact` reports the bytes `catalog.redb` was allocated before and after,
and what that difference reclaimed. It needs the catalog to itself: a catalog another process
holds is refused rather than waited for.

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
- `catalog status` reported `format_version: 1` and named every table `<model id>_1_<key>`.
  The format version is 2 from `969ebe4`; the native_model version of every record is still
  1, so the table names are unchanged.
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
incremental path is measured beside the full one. It puts those 1,000 headers back before it
returns, so the next binary measured against the same tree reads exactly the same input.

The harness closes the catalog three times, because redb's per-table statistics need the
exclusive lock: after the scan, after the queries, which is the state the rebuild left, and
after the incremental workload. Each close writes a JSON report beside the catalog and
prints it. Reopening drops native_db's 256 MiB redb cache, so the rebuild and the rescan run
against a cold record cache and a warm operating-system page cache; that is the same for
every column taken with this harness and is not comparable to the columns above it.

It then rebuilds the search index a second time, over the published one, which is the only
way to see what a rebuild costs in temporary disk: the fresh build has no superseded
directory beside it.

`supplemental_catalog_costs` in the same file is a second ignored test, behind the
`fixtures` feature, for the costs the one-artifact-per-file gate cannot show. It seeds
records rather than sniffing files: long relative paths, an hf-cache source whose entries
carry an explicit blob path, populated sha256 and git blob shas, a full model card and
tensor summary per artifact, three revisions per repository, and one artifact in ten reached
by more than one path.

```sh
HOARD_SCALE_DIR=<scratch> cargo test --release -p hoard-index --features fixtures \
  --test scale -- --nocapture --ignored supplemental
```

At 1M. `95cd3ad` is the base this wave started from and `1783f2b` is the wave as it landed;
both were measured in one session on this workstation with the harness at `ef29745`, on an
otherwise idle machine. Exact bytes, with MiB beside them.

| measure | 95cd3ad | 1783f2b |
| --- | --- | --- |
| scan wall | 98.808 s | 76.395 s |
| peak rss growth during scan | 879,169,536 (838.44 MiB) | 880,521,216 (839.73 MiB) |
| rebuild wall | 13.634 s | 7.512 s |
| peak rss growth during rebuild | 343,195,648 (327.30 MiB) | 351,121,408 (334.86 MiB) |
| on disk after rebuild, allocated | 2,387,607,552 (2277.00 MiB) | 1,399,648,256 (1334.81 MiB) |
| of which `catalog.redb` | 2,304,364,544 | 1,341,190,144 |
| of which the search directory | 83,243,008 | 58,458,112 |
| record payload after rebuild | 1,042,201,188 (993.92 MiB) | 563,971,405 (537.85 MiB) |
| on disk after the churn, allocated | 2,387,750,912 | 1,403,404,288 |
| query empty by name | 117.374 ms | 119.359 ms |
| query deep offset | 118.702 ms | 118.851 ms |
| query midpoint offset | 1.626 s | 1.529 s |
| query role:lora with facets | 141.044 ms | 143.290 ms |
| query filtered | 225.676 ms | 229.826 ms |
| query node:self with facets | 474.983 ms | 408.324 ms |
| inspector artifact_detail | 25.296 us | 25.424 us |
| inspector entries_of | 34.144 us | 30.576 us |
| inspector artifact_row | 9.104 us | 8.736 us |
| rescan wall (1,000 changed) | 5.134 s | 5.151 s |
| refresh wall | 26.137 ms | 27.493 ms |
| rebuild wall over a published index | 9.434 s | 7.847 s |
| peak allocated while an index was replaced | 2,476,257,280 | 1,463,869,440 |

The disk rows and the payload row are the change, and so are the scan and the two rebuilds.
The scan is 22.7 per cent faster because there are fewer bytes to serialize and write, a
million change-log rows are no longer among them, and a unit is prepared outside the write
transaction. The first rebuild is 44.9 per cent faster because the artifact record it already
holds is no longer looked up and decoded a second time, there are fewer search columns to
write, and publishing no longer prunes a million log rows.

`node:self` moved from 474.983 ms to 408.324 ms, and read 352.126 and 438.563 ms at
`2c6bde6`. The direction is right and the mechanism is that six keyword columns no longer
exist for the collapse to open, but a spread of 86 ms across the after runs means the size is
not established.

The four other queries, all three inspector rows, the rescan and the refresh moved by less
than the spread of their own runs and are not results.

`peak rss growth during rescan and refresh` is omitted. It is a high-water mark attributed to
a phase by subtraction and read 65,220,608, 313,569,280, 95,752,192 and 72,101,888 bytes
across four runs of two commits; a 4.8x spread measures the allocator, not the change.

**What the change log cost.** At `2c6bde6` a fresh scan wrote one `SearchChange` row per
artifact and the rebuild that followed deleted every one of them unread, because a build
reads the artifact records rather than the log. At 1M the table was 32,000,000 stored bytes
over 80,171,008 bytes of pages, and redb does not return freed pages to the operating system,
so the file kept them. `1783f2b` writes none: allocated disk after the rebuild went from
1,480,302,592 to 1,399,648,256 bytes, 80,654,336 less, and the residual `catalog.redb` holds
beyond its tables fell from 87.68 MiB to 11.19 MiB. At 100k the same fix is worth 7,655,424
bytes.

At 100k, the same pair: the whole directory 243,363,840 to 143,675,392 bytes, `catalog.redb`
235,188,224 to 137,908,224, the search directory 8,175,616 to 5,767,168, record payload
103,123,417 to 55,512,438, scan 6.461 s to 5.019 s, rebuild 1.248 s to 0.836 s. The 100k
queries and inspector rows all moved by less than a millisecond and are not results.

Allocated disk is `du` over the output directory; apparent disk sums file lengths, and redb
grows `catalog.redb` in regions, so the apparent number is quantised and the allocated one is
what the disk actually loses.

### A page anywhere in the order, at 1M

The paging wave, measured in one session on this workstation over one tree, on an otherwise
idle machine. All three are checkpoints of the `wave/page-anywhere` branch that `master`
carries as one commit: `55330c2` is the harness with its new rows on the previous search
code, `d91302a` is the sampled selection as first written, and `2552309` is it after the
performance review that followed.

| measure | 55330c2 | d91302a | 2552309 |
| --- | --- | --- | --- |
| segments after rebuild | 3 | 3 | 3 |
| query empty by name | 119.410 ms | 124.878 ms | 121.149 ms |
| query deep offset | 119.454 ms | 125.401 ms | 120.742 ms |
| query midpoint offset | 1.508 s | 204.113 ms | 184.532 ms |
| query midpoint offset by size | 225.749 ms | 173.865 ms | 170.737 ms |
| query midpoint offset after refresh | 1.544 s | 216.370 ms | 189.565 ms |
| segments after refresh | 4 | 4 | 4 |
| query role:lora with facets | 141.857 ms | 149.102 ms | 146.527 ms |
| query filtered | 229.220 ms | 240.957 ms | 236.601 ms |
| query node:self with facets | 411.839 ms | 381.425 ms | 322.365 ms |
| resident rss over the queries | 17,113,088 (16.32 MiB) | 16,314,368 (15.56 MiB) | 16,314,368 (15.56 MiB) |
| resident rss over the midpoint queries | 61,407,232 (58.56 MiB) | 13,754,368 (13.12 MiB) | 14,278,656 (13.62 MiB) |
| peak rss growth during the queries | 0 | 0 | 0 |
| scan wall | 75.003 s | 75.241 s | 76.749 s |
| rebuild wall | 7.752 s | 8.251 s | 7.343 s |
| on disk after rebuild, allocated | 1,398,964,224 | 1,398,095,872 | 1,398,759,424 |

The three midpoint rows and the resident memory over them are the change. A page in the
middle of the default order fell from 1.508 s to 184.5 ms, 8.2x, and the same page over the
four segments a refresh leaves fell from 1.544 s to 189.6 ms; the memory the two midpoints
hold above their own page fell from 58.56 MiB to 13.62 MiB. The midpoint by size fell from
225.7 ms to 170.7 ms, a smaller effect because that key never spelled a term: it paid only
for the heap, and now pays for two extra passes instead.

Two costs the numbers separate. The 1.5 s was not the heap: sorting by name over three
segments spelled a term for every one of the 500,050 rows the heap held, and that spelling
was most of it, which is why the midpoint by size was 225 ms on the same window. The sampled
selection removes both, because the bucket it spells from holds about a thousand rows rather
than half the match.

What the review's four fixes were worth, `d91302a` to `2552309`: 19.6 ms off the name
midpoint and 26.8 ms off the same page after a refresh, which is the sample falling from a
fixed 4,096 boundaries to the square root of the match, 1,000 of them, each of which had cost
a dictionary seek in every segment that did not hold its term. The gap between the two
midpoints, 12.3 ms at `d91302a` against 5.0 ms at `2552309`, is that per-segment seek cost
and it shrank with the sample. The rest of the fixes, transposing the boundaries into one
contiguous table per segment and reading only the sort column and the artifact id in the
bucket pass, are inside the 184.5 ms and were not separately timed.

`query empty by name`, `query deep offset`, `role:lora` and the filtered query took 5 to
12 ms at `d91302a` that the review recovered most but not all of: against `55330c2` they now
stand at +1.7, +1.3, +4.7 and +7.4 ms. The first two are inside the spread of their own runs;
the last two are not, and are a 3.2 per cent regression on two rows this wave did not put on
a new path. All four are shallow windows that still take the heap. At 100k the same four rows
are back on their before numbers to within 0.7 ms, so whatever is left does not scale with
the change; the reviewer refuted the three mechanical explanations. Codegen around the
collector's inner loop is the hypothesis that remains, and it is only that: no profile has
been taken that establishes it.

`node:self` is the collapse path, which this wave did not touch; it read 411.839, 381.425 and
322.365 ms across the three columns, a spread wider than any difference between them. The
scan, rebuild and disk rows are the same workload as before and moved by less than their own
spread.

At 100k, `55330c2` to `2552309`: `query midpoint offset` 104.014 ms to 19.398 ms, `query
midpoint offset by size` 15.564 ms to 14.623 ms, `query midpoint offset after refresh`
111.448 ms to 20.654 ms, resident memory over the midpoints 1.09 MiB to 1.17 MiB. The 100k
index is two segments after the rebuild and three after the refresh, and a 100k midpoint by
size is a window of 50,050 rows, which is inside the numeric crossover below and so still
takes the heap. Nothing else at 100k moved by more than a millisecond.

**The sample must not resonate with the order documents were written in.** The stride was
`counted / boundaries`, an even number for every round match, and the synthetic tree writes
files whose size repeats with a period of 32. A stride of 1,000 shares a factor of 8 with
that period, so the sample only ever landed on four of the 32 size classes and 28 of them
got no boundary at all: with 1,002 boundaries the median bucket held 126 rows and the largest
held 80,835, over the 65,536 cap, so a size-sorted page at some offsets silently fell back to
the heap and paid for both. Forcing the stride odd makes it coprime with every power of two;
the same measurement then reads a median of 657 and a largest bucket of 8,619, and the four
size-sorted crossover offsets that had read 177.0, 177.6, 331.5 and 322.1 ms read 175.9,
176.1, 175.4 and 174.5 ms. Nothing about an answer depended on this - the equality tests hold
either way - only what it cost.

### Tallying by ordinal and reading by block

The tally wave, measured in one session on this workstation over one tree, on an otherwise
idle machine, with nothing else running. `95d6fe0` is the head the wave started from,
`d6897a2` is the per-segment vector tallies, `286784c` is those plus the block reads, and
`e2e1809` is the head after the performance review that followed: blocks that survive
deletes, and the collapse, window and bucket collectors blocked as well. The two 1M columns
either side of the review are the first of two runs each; `e2e1809` is one run.

| measure, 1M | 95d6fe0 | 286784c | e2e1809 |
| --- | --- | --- | --- |
| segments after rebuild | 3 | 3 | 3 |
| query empty by name | 120.306 ms | 21.153 ms | 21.143 ms |
| query deep offset | 120.761 ms | 21.436 ms | 21.448 ms |
| query midpoint offset | 184.543 ms | 86.762 ms | 71.963 ms |
| query midpoint offset by size | 170.409 ms | 72.186 ms | 58.298 ms |
| query role:lora with facets | 146.224 ms | 24.106 ms | 24.182 ms |
| query filtered | 234.624 ms | 38.370 ms | 38.341 ms |
| query node:self with facets | 338.563 ms | 178.317 ms | 120.578 ms |
| query midpoint offset after refresh | 189.095 ms | 110.939 ms | 76.649 ms |
| segments after refresh | 4 | 4 | 4 |
| resident rss over the queries | 56,238,080 (53.63 MiB) | 56,242,176 (53.64 MiB) | 16,314,368 (15.56 MiB) |
| resident rss over the midpoint queries | 14,278,656 (13.62 MiB) | 14,278,656 (13.62 MiB) | 14,278,656 (13.62 MiB) |
| peak rss growth during the queries | 0 | 0 | 0 |
| scan wall | 74.261 s | 77.286 s | 75.719 s |
| rebuild wall | 7.516 s | 8.799 s | 7.627 s |
| on disk after rebuild, allocated | 1,399,234,560 | 1,399,762,944 | 1,398,345,728 |

| measure, 100k | 95d6fe0 | d6897a2 | 286784c | e2e1809 |
| --- | --- | --- | --- | --- |
| query empty by name | 12.569 ms | 3.935 ms | 2.734 ms | 2.711 ms |
| query deep offset | 12.822 ms | 4.209 ms | 2.973 ms | 2.951 ms |
| query midpoint offset | 19.101 ms | 10.782 ms | 9.462 ms | 7.980 ms |
| query midpoint offset by size | 14.441 ms | 5.849 ms | 4.773 ms | 4.617 ms |
| query role:lora with facets | 14.995 ms | 4.271 ms | 2.878 ms | 2.844 ms |
| query filtered | 23.596 ms | 6.553 ms | 4.203 ms | 4.164 ms |
| query node:self with facets | 23.636 ms | 7.439 ms | 7.907 ms | 6.333 ms |
| query midpoint offset after refresh | 20.478 ms | 12.023 ms | 13.286 ms | 9.336 ms |
| resident rss over the queries | 1.93 MiB | 1.90 MiB | 1.84 MiB | 1.86 MiB |

Every query row is the change. At 1M the first page of the default order fell from 120.3 ms
to 21.1 ms, 5.7x; the deep page with it; `role:lora` with its facet rail from 146.2 ms to
24.2 ms, 6.0x; the filtered query from 234.6 ms to 38.3 ms; the middle page from 184.5 ms to
72.0 ms; and `node:self`, the collapse path, from 338.6 ms to 120.6 ms, 2.8x. The 100k
columns split the mechanisms: of the 9.8 ms the first page lost, the per-segment vectors took
8.6 ms and the block reads 1.2 ms more.

**A block read is only worth anything if it survives deletes.** At `286784c` the collectors
overrode `SegmentCollector::collect_block` and nothing else, which tantivy calls only while a
segment has no delete file. `refresh_search` deletes an artifact's documents by term before
re-adding them, so the first refresh puts every touched segment on the per-document path -
where `collect` now cost *more* than the code it replaced, because one document went through
ten `clear`, `resize(1)` and `first_vals` calls on a one-element slice. The 1M row that shows
it is the midpoint after a refresh, which sat 4.6 ms above the plain midpoint before the wave,
24.2 ms above it at `286784c`, and 4.7 ms above it again at `e2e1809`. Overriding
`Collector::collect_segment` and doing the alive filtering there restores the block path on
segments with deletes and is worth 34.3 ms of that row at 1M and 4.0 ms at 100k.

Blocking the other three collectors is the rest of the review: the plain midpoint fell
14.8 ms and the midpoint by size 13.9 ms, which is the bucket and window passes, and
`node:self` fell 57.7 ms, which is the collapse collector reading four columns per document
before. `query empty by name`, `query deep offset`, `role:lora` and the filtered query are
unmoved either way, inside 0.1 ms, because they never take those passes.

The 1M spread between the two runs at each commit: at `95d6fe0`, 119.4 to 120.3, 119.4 to
120.8, 182.8 to 184.5, 168.5 to 170.4, 144.7 to 146.2, 232.5 to 234.6 and 320.3 to 338.6 ms;
at `286784c`, 21.0 to 21.2, 21.3 to 21.4, 85.6 to 86.8, 71.3 to 72.2, 23.7 to 24.1, 37.8 to
38.4 and 173.1 to 178.3 ms. Every difference above is far outside both.

Resident memory over the queries reads 53.63 and 15.56 MiB at `95d6fe0`, 53.64 and 53.66 MiB
at `286784c`, and 15.56 MiB at `e2e1809`: the same two values on both sides of the change. The
row is a `VmRSS` difference across the five queries and is dominated by how much of the search
directory the page cache has faulted in, so its own spread is 38 MiB wide and it measures
nothing about this wave. What the wave adds is bounded and countable: nine buffers of at most
64 slots per segment collector, about 9 KB per segment, plus one `u64` per term of each facet
dictionary per segment.

The scan, rebuild and disk rows are the same workload as before and are not on any path this
wave touches: a rebuild writes documents and runs no collector. Its wall read 7.627 to
8.799 s across the three columns against 7.108 to 7.516 s at `95d6fe0`, a difference the size
of the spread of its own runs. One row moved for a reason the change cannot reach: peak RSS
growth during the rescan and refresh read 227.98 MiB at `e2e1809` against 55.32 and 56.52 MiB
before it. Neither a rescan nor a refresh runs a collector; the row is a high-water-mark
difference measured after the queries, so a run that faulted less of the index in during the
queries leaves the refresh more room to raise the mark. The absolute peak, the 864 MiB the
scan grows by, is the same in every run.

### Thresholds

At 1M entries, absolute: peak RSS growth during the scan under 2 GB, allocated disk under
1,500,000,000 bytes, every query under 500 ms, rebuild no slower than the scan. Measured at
`1783f2b`; the disk rows are the same to within 37 KB across three runs, the rest is one run
on an otherwise idle machine.

| gate | measured | verdict |
| --- | --- | --- |
| allocated disk after the rebuild | 1,399,648,256 bytes | met, by 100.4 MB |
| allocated disk after the incremental workload | 1,403,404,288 bytes | met, by 96.6 MB |
| allocated disk at the peak of a rebuild that replaces a published index | 1,463,869,440 bytes | met, by 36.1 MB |
| peak RSS growth during the scan | 880,521,216 bytes | met |
| rebuild no slower than the scan | 7.51 s against 76.39 s | met |
| every query on the original list under 500 ms | slowest is `node:self` at 474.98 ms before, 408.32 ms after | met |

Three rows the original list did not carry, measured at `2552309` against `55330c2` on the
same tree in one session:

| gate | measured | verdict |
| --- | --- | --- |
| a page in the middle of the default order under 500 ms | 184.532 ms, from 1.508 s | met, by 315.5 ms |
| the same page after a refresh, over four segments | 189.565 ms, from 1.544 s | met, by 310.4 ms |
| a page in the middle by size under 500 ms | 170.737 ms, from 225.749 ms | met, by 329.3 ms |
| the first page and the last page unmoved | +1.7 ms and +1.3 ms | met, inside their own spread |
| `role:lora` and the filtered query unmoved | +4.7 ms and +7.4 ms, 3.2 per cent | **missed**, outside their spread |
| `node:self` unmoved | 411.8, 381.4 and 322.4 ms across three commits | met, the spread exceeds the difference |
| resident memory over the queries no higher than before | 15.56 MiB against 16.32 MiB | met |

This wave's rows, measured at `e2e1809` against `95d6fe0` on the same tree in one session:

| gate | measured | verdict |
| --- | --- | --- |
| `query empty by name` at least 40 per cent faster | 21.143 ms from 120.306 ms, 82.4 per cent | met |
| `query deep offset` at least 40 per cent faster | 21.448 ms from 120.761 ms, 82.2 per cent | met |
| `query role:lora with facets` at least 40 per cent faster | 24.182 ms from 146.224 ms, 83.5 per cent | met |
| `query filtered` at least 40 per cent faster | 38.341 ms from 234.624 ms, 83.7 per cent | met |
| the three midpoint rows no slower than their spread | 71.963, 58.298 and 76.649 ms from 184.543, 170.409 and 189.095 ms | met, 61, 66 and 59 per cent faster |
| `node:self` no slower than its spread | 120.578 ms from 338.563 ms | met, 64 per cent faster |
| resident memory over the queries no higher | 15.56 MiB against a before column of 53.63 and 15.56 MiB | met |
| every query on the original list under 500 ms | slowest is `node:self` at 120.578 ms | met, by 379.4 ms |

**Where the heap stops paying.** `Paging::DEFAULT.shallow` is the window above which a rank
selection replaces the heap, and it is 16,384 rows for a text key and eight times that for a
numeric one. Measured at 1M on the tree above with two release builds of the harness, one
forced to the heap and one forced to the sample, each run once, each offset timed warm after
the fixed list had already run; these numbers are therefore not comparable to the table
above, only to each other.

| window | heap, text | sampled, text | heap, numeric | sampled, numeric |
| --- | --- | --- | --- | --- |
| 8,292 | 147.209 ms | 189.469 ms | 127.316 ms | 175.926 ms |
| 32,868 | 228.059 ms | 189.610 ms | 140.856 ms | 176.105 ms |
| 131,172 | 476.414 ms | 188.873 ms | 175.330 ms | 175.431 ms |
| 500,050 | 1.386 s | 186.409 ms | 141.718 ms | 174.519 ms |

The sampled columns are flat, which is the property the wave was for: what a page costs does
not depend on where in the order it sits. The heap column is what crosses them. A text key
crosses between 8,292 and 32,868, at about 20,900 rows by interpolation, because the heap
spells a term for every row it keeps; 16,384 is the power of two under that. A numeric key
compares integers and does not cross until 131,172, where the two are level to 0.1 ms;
131,072 is the constant, and `NUMERIC_SHALLOW` is the factor of 8 between them.

Past that point the numeric heap is not monotone - it reads 175.330 ms at 131,172 and
141.718 ms at 500,050 - so at the midpoint a numeric heap is about 33 ms faster than the
sample on a single warm sample. It is not chosen there: that heap holds 500,050 rows in every
segment it reads, which is the 58.56 MiB the wave removed, against about a thousand rows for
the bucket. Where the two are level in time the bounded one is taken - except below the caps,
where the sampled selection falls back to the heap on purpose, so paging is bounded
everywhere above them and not below.

The first two rows are the catalog at rest. The third is not: `publish` writes the new
`search-v1-<nanos>` directory in full and keeps the old one until the new one is committed
and named in the manifest, so for the length of a rebuild the disk holds two search
directories. That transient is one search directory, 58,458,112 bytes, above the resting
figure, and it is the number to watch, because it was **over** the gate at 1,539,854,336 and
1,541,668,864 bytes one commit earlier and only came under it when the change log stopped
being written. Nothing here redesigns `publish`: an unpublished build leaving the previous
one serving is what makes a failed rebuild harmless.

The disk gate was missed by 887,607,552 bytes at `95cd3ad` and is met by 100.4 MB at
`1783f2b`, a 41.4 per cent reduction.

Standalone query-process resident memory over the five queries is 71.50 MiB, against zero
high-water growth: a query allocates, the process had simply already been higher during the
rebuild. Zero high-water growth after a scan is not proof that a query allocates nothing.

### Where the bytes are

Measured with `crates/hoard-index/tests/scale.rs`, which closes the catalog and reads redb's
own per-table statistics after the scan, after the rebuild and after the incremental
workload, and writes each report to `storage-<entries>-<phase>.json` beside the catalog.

Three numbers per table, and they mean different things:

- **stored** is the key and value payload of the rows, which is what a record encoding
  changes.
- **metadata** is the branch keys and page headers around it. For a secondary index the
  stored column is only the 8 byte primary keys it points at, so the index key itself is
  counted here.
- **pages** is `(leaf + branch) * 4096`, the space the file actually spends. Fragmentation
  is the difference: unused room inside pages redb has already allocated, mostly ordinary
  B-tree fill factor rather than garbage a compaction is promised to return. Every large
  table sits near 45 per cent fragmented straight after a fresh scan, which compacts, and
  that ratio did not move: halving a payload halves the pages, it does not pack them
  tighter.

At 1M entries, after the rebuild. `95cd3ad` is the base this wave started from, measured with
the same harness; `1783f2b` is the wave. Both columns were taken in one session on this
workstation.

| table | rows | pages at `95cd3ad` | pages at `1783f2b` | stored at `1783f2b` | leaf fill |
| --- | --- | --- | --- | --- | --- |
| `8_1_artifact_id`, `ArtifactDetail` | 1,000,000 | 869.82 MiB | 497.00 MiB | 267.52 MiB | 54.8% |
| `3_1_id`, `ArtifactRecord` | 1,000,000 | 424.43 MiB | 278.74 MiB | 136.11 MiB | 49.7% |
| `7_1_id`, `Entry` | 1,000,000 | 397.49 MiB | 164.78 MiB | 75.00 MiB | 46.3% |
| `7_1_location`, the unique location index | 1,000,000 | 156.39 MiB | 118.08 MiB | 7.63 MiB | 6.7% |
| `5_1_artifact_id`, `SearchText` | 1,000,000 | 119.28 MiB | 88.23 MiB | 36.33 MiB | 41.9% |
| `7_1_artifact_id`, the artifact index | 1,000,000 | 63.11 MiB | 63.11 MiB | 7.63 MiB | 12.3% |
| `3_1_identity`, the identity index | 1,000,000 | 58.13 MiB | 57.89 MiB | 7.63 MiB | 13.4% |
| `10_1_seq`, the change log, after the scan | 1,000,000 then 0 | 80,171,008 bytes | never written | | |
| `7_1_repo_id`, the repository index | 1,000,000 | 23.25 MiB | gone | | |

The seven surviving tables came to 2,214,522,880 bytes of pages and now come to
1,329,451,008, against 1,341,190,144 bytes allocated for `catalog.redb`: the 11.19 MiB
residual is redb's own allocator state, its freed-page tables and the region headers. That
residual was 87.68 MiB while the change log was written and freed, because redb does not
give a freed page back to the operating system. The published Tantivy directory is
58,458,112 bytes, 55.75 MiB, down from 83,243,008.

Per row, in payload bytes: `ArtifactDetail` 516 to 272, `ArtifactRecord` 218 to 135, `Entry`
197 to 71, `SearchText` 46 to 30. The location index key averaged 76 bytes and now averages
55: it lost 40 bytes of zero-padded decimal and gained the relative path unchanged.

The largest remaining cost is `ArtifactDetail` at 497.00 MiB of pages, 37.1 per cent of
`catalog.redb`, of which 267.52 MiB is payload and 217.01 MiB is fill factor. On this fixture
each row is one tensor summary of a single tensor plus its evidence, and LZ4 takes 7 per cent
off it; on a fixture with real cards and real tensor metadata it takes 75 per cent off. The
second largest is `ArtifactRecord` at 278.74 MiB.

**Intra-page slack is now the largest single term.** Across every table it is 608,835,673
bytes, 45.4 per cent of `catalog.redb`, and the four record tables sit at 42 to 55 per cent
leaf fill. That is ordinary B-tree occupancy after a pass of random-order inserts, and
`Database::compact` does not reclaim it: compaction rewrites the file without its free pages
and leaves the pages that hold rows exactly as full as they were. Halving a payload again
would halve the pages again, but the fill factor would not move. See "Remaining work".

At 100k the same order and ratios hold: the whole directory is 143,675,392 bytes,
137.02 MiB, against 243,363,840 at `95cd3ad`.

### What the one-artifact-per-file fixture cannot show

`supplemental_catalog_costs` seeds 200 repositories of 3 revisions of 50 files: long relative
paths, an hf-cache source whose entries carry an explicit blob path, populated sha256 and git
blob shas, a full model card and tensor summary per artifact, and one artifact in ten reached
by more than one path. 30,000 entries, 27,300 artifacts. It needs `--features fixtures`;
`-p hoard-index` alone leaves the module out and the command reports success over zero tests.

| measure | records compact, search untrimmed | `1783f2b` |
| --- | --- | --- |
| whole directory, allocated | 311,848,960 bytes | 134,791,168 bytes |
| `catalog.redb`, allocated | 281,985,024 bytes | 109,887,488 bytes |
| `SearchText` pages | 113.82 MiB | 28.50 MiB |
| `ArtifactDetail` pages | 113.82 MiB | 28.50 MiB |
| record payload | 193,738,011 bytes | 53,353,221 bytes |
| inspector `artifact_detail` | 81.4 us | 45.1 us |
| seed wall | 2.225 s | 1.606 s |

Compressing the two cold tables is what does that: on a catalog whose artifacts carry cards
and tensor metadata it takes 56 per cent off the whole directory and makes the inspector
faster, because there is less to read. On the no-card gate fixture above it takes 7 per cent
off `ArtifactDetail`'s payload, which is not enough to change its page count, and costs one
tag byte per `SearchText` row. Both numbers are real; the second is what the gate measures
and the first is what a catalog of real files would.

### Units

Runs before this one divided byte counts by 1024 squared and labelled the result MB. Every
number here is exact bytes with its MiB conversion beside it. The disk gate this document
tracks is stated in bytes: **1,500,000,000**. 1.5 GiB would be 1,610,612,736, so the two
readings of the old threshold are 110 MB apart and the byte form is the one used.

## Remaining work

- **A primary query counts its match twice.** `Selecting::of` runs a `Count` search to learn
  the total before `Window::of` can say which slice of the order was asked for, and the
  tallying pass that follows counts the same documents again in `Counting::count`. Now that a
  tallying pass costs about what a counting pass costs, that first pass is about half of what
  a shallow primary query spends: 21.1 ms at 1M for the two passes over a match of a million
  documents. A sampled selection pays it as one of four passes. Removing it means the window
  deciding what it needs after the collection rather than before, which is a change to
  `Selecting` and `Window`, not to the collectors. This is the largest cost left on the
  primary path.
- **`Catalog::facets` runs one `Count` search per distinct base value in the answer**, with
  no cap. The prefix rule that groups base labels means the exact-term count the tally holds
  is not the count a click on that bucket produces, so each bucket is counted by running its
  own query. `Base` is open ended, so the number of buckets is bounded by what the catalog
  holds and not by its size: at 200 distinct bases that is 200 extra searches per request. The
  synthetic tree carries three bases, so nothing measured here exercises it. The two ways out
  are named under "Decisions someone has to take" in `docs/handoff.md`; neither is an
  implementation detail.
- **A tally holds a slot per term of the column, not per term the match used.**
  `SegmentTallies::sized_for` allocates `num_terms()` slots per facet dimension per segment,
  which is eight bytes per base value per segment whether the query matched it or not. The
  five facet columns are small closed vocabularies except `base`, so a catalog with a hundred
  thousand distinct bases would allocate 800 KB per segment per pass where the old map
  allocated per value matched, and the review put the zeroing alone at about 12 ms per
  request at that cardinality. It is below the noise at any plausible base count today; it is
  the trade the vector makes and the number to watch if base cardinality ever grows.
- **The base facet runs one query per distinct base value.** The prefix rule that groups base
  labels cannot be answered from ordinals, so `facets` re-runs each bucket's own query to
  count it. The synthetic tree carries three bases, so no measurement here exercises it; a
  catalog with hundreds of bases pays hundreds of searches per query.
- **The cap's fallback recollects rather than reusing the sampling pass.** When the chosen
  bucket is over 65,536 rows the heap answers the whole request, and the tallies and node set
  the sampling pass already computed are dropped and computed again. It is accepted as it
  stands: reaching it needs a bucket that dodged every sample slot, which the odd stride
  makes improbable, and the alternative is a second way to assemble a `Summary` for a path
  that should not run.
- **Bound the collapse the way the primary path is bounded.** A query naming `node:`,
  `state:`, `source:`, `repo:`, `rev:` or `hash:`, or sorting on revision, node or state,
  still keeps one row per matching artifact rather than one per page. At 1M `node:self` is
  352 to 439 ms, under the 500 ms threshold rather than bounded by design.
- **Shrink `ArtifactDetail` further.** It is 497.00 MiB of 1,421,844,480 bytes at 1M, 35.0
  per cent of `catalog.redb` and the largest table by a wide margin, of which 217.01 MiB is
  B-tree fill factor rather than payload. LZ4 takes 75 per cent off a row carrying a real
  card and real tensor metadata and 7 per cent off this fixture's one-tensor summary, so
  what is left is the summary itself: samples, key prefixes and verbatim metadata, per
  artifact rather than per revision.
- **Share a card between the artifacts of one revision.** `SearchText` holds the revision's
  README per artifact. On the supplemental fixture that is 104,968,250 payload bytes
  compressed to 17,335,378; sharing it would remove the copies rather than compress them.
- **Pack the pages.** Intra-page slack is the largest single term left in `catalog.redb`:
  608,835,673 bytes at 1M, 45.4 per cent of the file, with the four record tables at 42 to
  55 per cent leaf fill. It is ordinary B-tree occupancy after a pass of inserts in primary
  key order interleaved across tables, and `Database::compact` does not reclaim it: it
  rewrites the file without its free pages and leaves the pages holding rows as full as they
  were. The lever is a bulk load or a dense rewrite that fills leaves before it seals them,
  not another codec. Another codec would halve the payload and halve the pages with it,
  leaving the same 45 per cent slack in half the file.
- **`sweep_artifact` decompresses a cold row to delete it.** native_db 0.8.2 has no
  remove-by-primary-key: `remove` takes the value, so the sweep reads `ArtifactDetail` and
  `SearchText` in full, LZ4 and bincode included, only to hand them straight back. Every
  artifact a pass orphans pays a decode it does not use. A `remove_by_key` upstream, or a
  cheaper path that reads the raw bytes, would take it off the rescan.
- **Compact the remaining records.** `Compact` and `Cold` are on the four tables that carry
  the bulk. `Repository`, `Source`, `Node`, `Usage`, `SearchChange` and the user-data rows
  still take native_model's fixed-width default. None of them is large now that the change
  log is not written before the first publish.
- **`native_model::Model::native_model_decode` unwraps** a payload shorter than its eight
  byte header rather than returning an error, so a truncated row panics. `codec.rs` cannot
  see that byte range; it is reached before any codec is called.
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
- **Add process-kill and power-loss recovery tests.** The current tests check transaction
  abort and publication boundaries, not power loss.

## Dependency and serialization findings

`native_db 0.8.2` requires exactly `native_model 0.4.20` and resolves to `redb 2.6.3`. Legacy
native_db upgrade features are disabled. `tantivy 0.26.1` uses only mmap and LZ4 features.
These are intentional constraints, not latest-major assumptions.

Hoard's row ids previously always used Serde's `deserialize_any`, which bincode cannot decode.
A binary deserializer now reads a row id as an integer and a hash as bytes, and a
human-readable one keeps reading and writing the prefixed string and the hex; JSON also keeps
accepting an integer id. `bincode 1.3` and `lz4_flex 0.13` are direct dependencies of
`hoard-index` for `catalog/codec.rs`: bincode is the major version native_model 0.4.20
already resolves to, and lz4_flex is taken with `default-features = false` and the safe
encode, decode and checked-decode features rather than through Tantivy's copy of it. The
persistence and reopen tests exercise both codecs, and `codec.rs`'s own tests exercise what
they refuse.
