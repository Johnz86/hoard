---
name: hoard-catalog
description: Facts about hoard's native_db + Tantivy catalog that an agent needs before changing crates/hoard-index/src/catalog - the module map, the write path, native_db 0.8.2 and Tantivy 0.26 semantics, and where the compatibility policy lives.
allowed-tools: Read, Grep, Glob, Bash
---

The catalog is hoard's index. `serve`, `scan`, `ls`, the mirror, the git face and gossip all
read and write it, and nothing else persists what hoard knows about a file. It lives in
`crates/hoard-index/src/catalog/`, opens at `<data_dir>/native`, and holds `catalog.redb`
plus one published `search-v1-<nanos>/` Tantivy directory.

Read `docs/native-catalog.md` for the current state and the measured numbers. This skill is
the part that is not in the code and is easy to get wrong.

## Compatibility

Read the "Compatibility" section of `CLAUDE.md` before changing any record shape. It is the
only statement of the policy; nothing here restates it.

## Module map

One line per file in `crates/hoard-index/src/catalog/`:

- `mod.rs` open, `open_or_scan`, status, integrity, `FORMAT_VERSION`, storage report, compact.
- `codec.rs` the compact and cold record encodings, named by `#[native_model(with = ...)]`.
- `models.rs` every `native_model` record: `Source`, `Repository`, `Revision`,
  `ArtifactRecord`, `ArtifactDetail`, `Entry`, `SearchText`, `RevisionText`, `SearchChange`,
  `Manifest`, `Summary`, `Allocator`, `Node`, `Usage`, `RepoUsage`, and the user-data rows.
- `scan/mod.rs` the pass itself: `scan_sources`, `rescan`, the walk loop, the summary, `CHUNK`.
- `scan/counters.rs` `Counters`, `DerivedShift`, the summary parts, `log_changes`, id allocation.
- `scan/walk.rs` the walk state and the unchanged-or-changed decision for a revision and a unit.
- `scan/plan.rs` discovery to units to prepared entries: shard sets, documents, roles, sniffing.
- `scan/store.rs` one entry, its artifact and its repository row written in one transaction.
- `scan/sweep.rs` the artifact sweep and every retirement of a revision, a source or a row.
- `scan/sources.rs` the source rows, their scanned counts and the configuration validation.
- `scan/revisions.rs` the revisions discovery found merged with the revisions the catalog holds.
- `naming.rs` the config and card a revision carries, artifact names, per-file hashes.
- `search/mod.rs` `QueryRequest`, `QueryPage`, `RefreshReport` and what the catalog imports.
- `search/schema.rs` `Fields`, the keyword and column constants, the document one entry writes.
- `search/build.rs` the rebuild, the metered writer, per-entry documents, the directory publish.
- `search/refresh.rs` `refresh_search`, the change log and its cursor, the published directory.
- `search/answer.rs` a query from request to page: the path, the facet rail, the rows.
- `search/inspect.rs` the segment, delete and document counts the tests and diagnostics read.
- `search/paging.rs` the page window, the heap or sampled selection and its boundaries.
- `search/tally.rs` the facet tallies per segment and the base rail buckets.
- `search/collapse.rs` the per-artifact collapse, the spelled terms and the column blocks.
- `search/collectors.rs` the Tantivy collectors: primary, sampling, bucket, window, count.
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
- `tests/` the catalog's own tests: shared fixtures in `mod.rs`, then `scan`, `revisions`,
  `storage`, `codec`, `reads`, `mirror`, `counts`, `rescan`, `index`, `paging`, `collapse`,
  `usage`, `hashing` and `recovery`.

## The write path

Every writer of an entry commits through `scan::Counters`:

- `Counters::load` reads the persisted `Allocator`; `next_repo` and the sibling id
  helpers hand out ids from it, starting from the higher of what it holds and the highest id
  each table holds, so a pass killed before it saved cannot have an id handed out twice.
- `Counters::touch(transaction, artifact_id)` is called *before* an artifact record is
  written. It keeps what the record contributed to the summary beforehand, so the pass moves
  the summary by a difference rather than recounting every record.
- `Counters::derived` is a `DerivedShift`. Call `added(&entry)` before writing an entry and
  `removed(&entry)` before removing one or moving it out of presence. It accumulates each
  repository's present files and bytes, each source's owed hashes and each node's filed
  count, and is flushed in the transaction that writes the entries it counted, so a record
  above the entries is rewritten once per transaction rather than once per entry and a killed
  pass leaves no count behind the rows that moved it.
- `log_changes(transaction, counters)` writes one `SearchChange` row per artifact the pass
  touched, in the same transaction that made the change. The sequence continues past both
  the highest key the log holds and `Manifest.search_seq`. It writes nothing while no search
  directory is published: a build reads the artifact records, not the log, and publishing
  prunes the whole log unread. `Counters::load` reads that flag once per pass.
- `Counters::save(transaction)` flushes whatever `DerivedShift` is left and writes the
  allocator back.
- A pass that commits more than once sets `Manifest.pass_open` before its first commit and
  clears it in its last, because the totals and each scanned source's files and bytes are what
  a pass settles at its end. A run that finds the mark set counts both again first.
  `docs/native-catalog.md` under "Recovery" says what a kill at each stage leaves and what is
  accepted.
- A transaction that takes an artifact's last path calls `sweep_artifact` for it before it
  commits, so no committed state counts a path no entry backs. The sweep at the end of the
  pass is then a repeat.

**Every writer of an entry moves the derived counts.** A new writer that skips
`DerivedShift` leaves `Repository::present_files`, `Repository::present_bytes`,
`Source::pending_hashes` and `Node::filed` behind, and `hoard doctor`'s summary check is
what catches it. A new writer that skips `log_changes` leaves the search index stale with no
record that it is.

## native_db 0.8.2 semantics

The local clone at `/home/johnz86/GIT/native_db` settles any question about the API. These
are the facts that have caught agents out:

- A **secondary-key scan materialises every matching primary key** before yielding, and
  decodes every yielded row. Early exit saves nothing; `take(n)` on a secondary scan still
  pays for the whole match. Design around it, do not paginate on it.
- **Every write holds the serialised row in memory until commit.** A transaction's memory is
  the sum of the rows it writes, so batch in bounded chunks (`scan::CHUNK`) and never hold a
  whole source open.
- **`upsert` on an existing row removes and reinserts the row and every secondary key.**
  `insert` does not read first. Prefer `insert` when the row is known to be new.
- **Primary scans are lazy redb ranges**, so a primary scan streams and can exit early.
- **`len().primary::<T>()` is O(1).**
- **Integer keys are fixed-width big-endian**, so `start_with(i64)` is exact rather than a
  prefix guess. A `String` key's `start_with(prefix)` is a byte-prefix match.
- **Inclusive-end ranges are unsafe on primary and unique keys.** Use an exclusive end.
- **There is no durability knob; every commit fsyncs.** Fewer, larger transactions is the
  only lever.
- **One process per catalog.** redb takes an exclusive lock, so a second process fails to
  open rather than waiting.

## Tantivy 0.26 usage

- One document **per entry**, not per artifact: `node:laptop AND state:local` must match one
  entry satisfying both, and the preferred row must be chosen from the entries that matched.
- One document per artifact carries the **`primary` flag**: the entry a row would be
  assembled from. A query whose tokens and sort key touch only fields every entry of an
  artifact agrees on is intersected with that mark and needs no collapse. `node`, `state`,
  `source`, `repo`, `rev` and `hash`, and sorting on revision, node or state, still collapse.
- The collectors keep **term ordinals**, not terms. Ordinals are lexicographic inside a
  segment and meaningless across segments, so a cross-segment order merges on spelled terms.
- The outbox is the `SearchChange` table plus `Manifest.search_seq`. `refresh_search` replays
  the log after the durable cursor, one Tantivy commit per batch, deleting an artifact's
  documents by term and re-adding its entries, then prunes what it applied. It also prunes
  what the cursor has already passed, which is what a refresh killed before its own prune
  leaves behind.
- A rebuild writes a fresh `search-v1-<nanos>` directory and publishes its name in a redb
  transaction only after Tantivy commits; publishing then removes every superseded
  directory, and so does `Catalog::open`, because a killed build publishes nothing and would
  otherwise leave its directory for good. An unpublished build leaves the previous one
  serving.
- The writer is one thread with a 32 MiB budget.

## Before you change anything here

Grep for every symbol you intend to touch and read its callers. `docs/native-catalog.md`
describes the state at a named commit and goes stale; the code does not.
