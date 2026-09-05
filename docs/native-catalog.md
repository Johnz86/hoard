# Native catalog implementation status

This branch implements an executable native_db/Tantivy filesystem catalog. It is an
experimental vertical slice, not a complete replacement of Hoard's existing persistent
layer. The regular `serve`, `scan`, `ls`, mirror and gossip paths still use SQLite.

There is no SQLite import, migration or compatibility path. Create a fresh native
catalog by scanning sources. No existing data needs to be retained.

## Running it

The parent of the output directory must already exist. The output directory must be
new and outside every configured source.

```sh
cargo run -- catalog scan --directory /existing/path/native-catalog
cargo run -- --config hoard.toml catalog scan --source cache --directory /existing/path/native-cache
cargo run -- catalog search --directory /existing/path/native-catalog 'format:gguf'
cargo run -- catalog search --directory /existing/path/native-catalog 'role:lora AND size:[0 TO 400000000]'
cargo run -- catalog status --directory /existing/path/native-catalog
cargo run -- catalog rebuild --directory /existing/path/native-catalog
```

Search uses Tantivy query syntax. Bare words are ANDed; quoted phrases, `AND`, `OR`,
`NOT`, exact field filters and numeric byte ranges are supported. It does not yet
compile Hoard's existing search grammar, shelves or compatibility expressions.
Keywords use exact spelling. Results use descending size and ascending artifact ID;
`--limit` is capped at 1,000 and `--offset` selects the page.

Exact fields: `role`, `sub_role`, `base`, `format`, `precision`, `node`, `state`,
`source`, `repo`, `revision`, `org`, `hash`, `trigger`. `size` is a numeric byte count.
Free text indexes names, organizations, bases, triggers, tensor key prefixes and
the body of individual README artifacts. Trigger field filters currently match
the complete trigger string; they do not implement legacy substring matching.

## What is implemented

- Existing HF-cache, Git and loose-source discovery feeds the native store directly.
- Header sniffing and inference reuse the existing Rust implementations, with up to
  128 files prepared in a Rayon batch and one transaction per completed batch.
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
  transaction only after Tantivy commits. The previous directory remains available.
- A custom Tantivy collector produces exact distinct artifact totals and chooses the
  preferred entry after filtering. Only the result page is loaded from native_db.
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

Tantivy uses one writer thread with a 32 MiB indexing budget. Existing filesystem
discovery still materializes a source's repository/entry list; the new sniffing and
write stage does not add an unbounded whole-source batch.

## Remaining replacement work

- Convert the existing Store/Connection API and live server, scanner, mirror, cache
  eviction, gossip, watch and CLI callers to native transactions.
- Add mutations, incremental scan/retraction and a transactional search outbox with
  replay. This slice creates complete fresh catalogs; it does not update them.
- Implement user overrides/noise, shelves, loadouts, settings, projections, jobs,
  usage, peer cursors and the associated graph constraints/cascades.
- Preserve complete repository Git metadata and add stable cross-scan identities.
- Add full hash scheduling, progressive identities and content deduplication.
- Port neighboring config/card inference, shard grouping and missing-file policies
  from the existing scan pipeline. Current native inference uses each file's own
  header/config/card plus path evidence. Every shard is currently a separate file.
- Port the complete Hoard grammar, aliases, local-node resolution, exclusions,
  facets, compatibility, shelf expansion and every sorting mode.
- Add cleanup for superseded search directories. This slice deliberately keeps old
  generated directories; repeated rebuilds consume additional disk space.
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
