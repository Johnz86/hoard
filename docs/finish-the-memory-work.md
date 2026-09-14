# Memory work: delivered scope and remaining limits

Continuous hashing and metered index commits are delivered with repeated measurements,
correctness tests, and passing scale/concurrency gates. Read the current results in
[the native catalog report](native-catalog.md).

The following focused slice adds transaction-scoped summary folding, verified by rollback,
repeated-touch, snapshot-lifetime, and interrupted-recovery tests. It has no new scale
measurements; the previous performance evidence belongs to `51418ad`.

Cleanup now also consumes touched-ID sets directly, avoiding full-size vector copies and
requeueing during summary capture. Source-stat updates stream repository totals into
per-source accumulators. `scan.rs` is then split into `scan/`, eight modules by
responsibility, none over 700 lines; retirement lives in `scan/sweep.rs`.

Paging entry IDs during dropped-source retirement is the next candidate, and it needs a
measurement before it needs a fix. `retire_source` collects ids, not decoded rows, at eight
bytes each, and a native_db secondary scan materialises every matching primary key before
yielding, so paging cannot remove the dominant term. No harness phase drops a configured
source, so the cost has never been observed. Build that phase first and let its number decide.

This does not complete a strict memory bound for every workload. The remaining limits are
source-wide retirement bookkeeping, pending usage batches, and oversized artifact groups.
Do not restore all recovered waves or describe the full memory redesign as complete.

The immutable baseline is `bdf2964f954f2612c5a2a0ab4d33b9361aef8e00`. Recovery tag
`reverted/2026-09-07-predictable-memory` points to
`04adbd767d8cc9cbfbcf45ef1cf048bb168cba17`; it remains an experimental input.
Its format-8 recovery cannot safely replace a populated format-7 catalog.

Any further work must isolate its contribution: grouped discovery remains separate from
summary folding, and usage rekeying from reindex coalescing. Preserving
usage history, overrides, shelves, and pins is required before a schema change. Inconclusive
or unmeasured components stay out. Never use the user's catalog for experiments.

Keep the acceptance gates: at 1M entries, scan growth below 2 GB, allocated disk below
1,500,000,000 bytes, rebuild faster than scan, and every search below 500 ms, including the
slowest of eight simultaneous midpoint requests. Run matched release binaries serially,
without `RUSTFLAGS`, with asserted fixtures and independently seeded synthetic catalogs.
Save compact evidence and remove scratch data and worktrees when finished.

## The open items, and what closes each

Five items were opened here and one remains. Each closes by being fixed or by being measured
and recorded as not worth fixing; none closes by being deferred again. They keep their
original order, the closed one first with its result, then what blocks what.

**Items 2, 3 and 4 were mis-sized when they were written.** They name structures bounded by a
repository, a revision and a shard set, each worth single-digit megabytes at a pathological
size. Measured at `b10c30d`, a process that opens a 1M catalog and rescans peaks at an
absolute 866 to 871 MiB, and three components account for all but four per cent of it; see
"What a rescan alone holds" in [the native catalog report](native-catalog.md).

| component | bound | MiB at 1M |
| --- | --- | ---: |
| discovery: the `DiscoveredEntry` rows plus the walker's single `Vec<DiscoveredFile>` | one source | about 452 at its peak, 350 held |
| redb read cache, filled by `known_stamps` reading every `Entry` | `CACHE_BYTES`, nine tenths of 256 MiB | up to 230 |
| `walk.known` and `walk.seen` | every entry of every scanned source | about 147 |
| item 2, `retire_source`'s ids | one repository | 8 at a million-entry repository |
| item 4, `counters.pending` | one revision | 8 at a million-entry revision |
| item 3, an unchunked `Unit::Set` | one artifact's shards | under 1 |

**The one goal: a rescan holds one repository at a time, not one source.** The walk already
loops sources, then repositories, then revisions; `walk.paths` already holds one repository's
index at a time; and every lookup into `walk.known` is keyed by a hash seeded with the
repository's id, so nothing reads across repositories. The only pass-wide consumer is
`retract_absent`, which runs once at the end and not at all on an interrupt.

It lands as three waves. The first is done; the other two are ordered by what they remove.

1. **The vehicle, done at `b10c30d`.** `rescan_only_catalog` and `discovery_only_source` in
   the scale harness, and the attribution above.
2. **Discovery per repository, done in the wave commit that follows `b10c30d`.** A scan or
   rescan plans a source's repositories and reads, stores and walks them a window at a time.
   On the 1M tree filed one repository per folder (`HOARD_SCALE_PER_FOLDER=1`), the rescan peak
   went from 821.61 and 832.78 MiB at `b10c30d` to 477.62 and 477.53 MiB: met against 600, and
   57 MiB above the 420 estimate. The rescan wall moved by less than its spread, 4.239 s against
   4.256 s, and the scan wall went from 80.50 s to 77.10 s. A single repository of a million
   files is held whole by design, because shard sets and the unchanged-revision test span a
   revision, so the one-repository tree stays at 866.50 MiB against 875.59. The numbers are in
   `docs/native-catalog.md` under "The rescan peak after windowed discovery, at 1M".
3. **`known`, `seen` and retraction per repository, expected to take the per-folder peak from the
   measured 478 MiB to about 330 MiB.** Catalog only: load a repository's stamps when the walk
   reaches it, move the rows it did not see into a pass-wide list of missing rows, and drop the
   rest. Retraction stays at the end of the pass and is still skipped when the walk is
   interrupted, so "an interrupted walk retires nothing" holds exactly as written. The stored
   repositories the walk never reached, which today are retracted only because every stamp was
   loaded up front, are streamed and retracted at the end in `CHUNK` transactions;
   `a_rescan_retracts_more_paths_than_one_transaction_holds` already fails if they are not. The
   floor after it is the redb read cache plus one window of repositories. A single repository of
   a million files stays held whole, as it does after wave 2.

Items 2, 3 and 4 close against this measurement below.

1. **The concurrency gate: closed at `df0b103`, met, and it was never a lock.** Latency
   against thread count at 1, 2, 4 and 8 was taken at both 100k and 1M and is recorded in
   [the native catalog report](native-catalog.md) under "Query latency against concurrent
   thread count". The curve is flat at both scales. The slowest of eight simultaneous midpoint
   queries over the slowest of one is 1.01 and 1.01 at 100k and 1.18 and 1.14 at 1M over two
   runs each, where serialization would have put it near eight; the 1M rise is inside the
   spread of the one-thread row itself, which measured 76.0, 90.0 and 91.4 ms over three runs.
   This workstation is heterogeneous, ten Cortex-X925 at 3.90 GHz and ten Cortex-A725 at
   2.81 GHz, and every unpinned burst is bimodal for that reason alone; pinned to one class
   the curve is flat to within two per cent, 76.337 ms at one thread against 77.760 ms for the
   slowest of eight on the X925 cores and 127.891 against 129.686 ms on the A725 cores. The
   632 and 681 ms did not reproduce in any vehicle, including the full harness run that
   produced them, where the slowest of eight now measures 97.475 ms against 76.338 ms for the
   same page single-threaded. The gate line above binds exactly as written and is met by
   370 ms on the slowest reading taken, so it is left unchanged. No follow-on wave is owed.

2. **Dropped-source retirement: closed at `b10c30d`, measured and not worth a wave.**
   `retire_source` in `crates/hoard-index/src/catalog/scan/sweep.rs` holds one repository's
   entry ids at a time, eight bytes each: 8 KB for a fixture repository of 1,000 files and 8 MB
   for a repository of a million, against a rescan peak of 866 MiB, under one per cent at the
   pathological size. Paging it could not remove the larger term anyway, since a native_db
   secondary scan materialises every matching key before it yields. **Reopens if** a single
   repository approaches the size of a whole catalog, which the per-repository goal above
   would then have to bound first.

3. **Oversized artifact groups: closed at `b10c30d`, measured and not worth a wave.**
   `revision_units` chunks standalone files by `CHUNK` and never chunks a `Unit::Set`, so one
   artifact's shards are prepared and stored together. A shard set is hundreds of files, not
   millions: under 1 MiB of prepared entries against an 866 MiB peak. **Reopens if** a real
   model is found whose shard count reaches the tens of thousands.

4. **The pending change set: closed at `b10c30d`, measured and not worth a wave.**
   `log_revision_artifacts` pushes one id per entry of a revision into `counters.pending`
   before a single `log_changes`, so the set is bounded by the largest revision rather than by
   `CHUNK`, as recorded when this item was opened. At eight bytes an id that is 8 MB for a
   million-entry revision against an 866 MiB peak; the row inserts it causes are write volume,
   not memory, and are paid once per card change. **Reopens if** a card change on a very large
   revision is measured to stall a rescan, which is a latency question this item never was.

5. **Two files over the 1,500-line cap.** `crates/hoard-index/src/catalog/search.rs` at 4,062
   lines and `crates/hoard-index/src/catalog/tests.rs` at 11,603. **Closes when** both are
   split the way `scan.rs` was, by responsibility, with no sibling import changed and no test
   count moved.

Items 2, 3 and 4 are all about the same question and share a harness run; do not seed three
separate catalogs for them. Item 5 needs no measurement and blocks nothing, so it goes last.
