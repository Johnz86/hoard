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

Five items were opened here and four remain. Each closes by being fixed or by being measured
and recorded as not worth fixing; none closes by being deferred again. They keep their
original order, the closed one first with its result, then what blocks what.

**Items 2, 3 and 4 were mis-sized when they were written.** They name structures bounded by a
repository, a shard set and a revision, and each is worth single-digit megabytes at a
pathological size. They sit beside two structures that a rescan holds unconditionally and that
are an order of magnitude larger, neither of which was on this list. What a 1M rescan holds,
by size, with the struct widths taken from the compiler rather than estimated:

| structure | where | bound | at 1M |
| --- | --- | --- | ---: |
| the discovered source | `discover::discover` in `catalog/scan/mod.rs:257` | one source, every entry, with `rel_path` and `abs_path` both on the heap | ~254 MB |
| `walk.known` | `Catalog::known_stamps` | every entry of every scanned source | ~136 MB |
| `retract_absent`'s `missing` | `catalog/scan/mod.rs:483` | every known entry not seen | up to 48 MB |
| `walk.seen` + `counters.touched` | the pass | every entry, every artifact | ~36 MB |
| item 2, `retire_source`'s ids | `catalog/scan/sweep.rs` | one repository's entries | 8 MB |
| item 4, `counters.pending` | `log_revision_artifacts` | one revision's entries | 8 MB |
| item 3, an unchunked shard set | `revision_units` | one artifact's shards | under 1 MB |

`DiscoveredEntry` is 144 bytes plus two heap strings; `revision_units` chunks standalone files
by `CHUNK` and never chunks a `Unit::Set`. The measured scan peak of 866 MiB at 1M is
consistent with the first two rows being most of it.

**The one goal that follows: a rescan holds one repository at a time, not one source.** The
walk is already structurally per repository - it loops sources, then repositories, then
revisions - and `walk.paths` already demonstrates the pattern, carrying the comment "so no
more than one repository's index is ever held". Extending that to the two large rows is the
whole change, and `location_hash` already keys `known` by `repo_id`, so every lookup is
already scoped to the repository being walked.

It lands as three waves, each with a number before and after:

1. **The vehicle.** No fixture can show any of this today: the harness runs the scan and the
   rescan in one process, so the rescan's own peak is masked by pages the scan already made
   resident, and "peak RSS growth" of a phase is not an allocation. A standalone rescan-only
   test that opens a seeded catalog and reports the absolute peak of a process that does
   nothing else, on the model of `concurrent_catalog_queries`. **Nothing else starts until
   this prints a number.**
2. **`known`, `seen` and retraction per repository.** Catalog only, one subsystem. Load a
   repository's stamps as it is reached, retract what it did not see before moving on, drop
   both. Expected to remove the second and third rows and most of the fourth.
3. **Discovery per repository.** The `discover` crate, a separate subsystem and so a separate
   wave: split `discover` into finding the repository roots, which is cheap, and reading one
   repository's entries, which is not. `store_source` needs the roots up front to assign
   ordinals, and the roots are what it should get.

Items 2, 3 and 4 close against wave 1's number rather than against a wave of their own: an
8 MB structure beside a 390 MB one is measured and recorded as not worth doing, unless wave 1
says the sizes above are wrong. **That verdict is not licensed until the number exists.**

1. **The concurrency gate: closed at `39bb637`, met, and it was never a lock.** Latency
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

2. **Dropped-source retirement.** `retire_source` in `crates/hoard-index/src/catalog/scan/sweep.rs`
   collects every entry id of a repository before it chunks the deletes. No harness phase drops
   a configured source, so the cost has never been observed, and paging cannot remove the
   dominant term because a native_db secondary scan materialises every matching primary key
   before yielding. **Closes when** a phase that drops a source at 1M has run and the result is
   either a fix with a before and an after, or a recorded verdict that the ids cost eight bytes
   each and the item is not worth a wave.

3. **Oversized artifact groups.** The one remaining limit with a fixture hook already in the
   harness, `HOARD_SCALE_WIDE_CARD_ARTIFACTS`. **Closes when** it has been run at 1M and the
   number decides, on the same terms as item 2.

4. **The pending change set, which is not bounded by `CHUNK`.** `counters.pending` drains
   inside `log_changes` once per write transaction, which reads as a `CHUNK` bound and is not
   one: a transaction can cover a whole revision. `log_revision_artifacts` in
   `crates/hoard-index/src/catalog/scan/sweep.rs` scans every entry of a revision and pushes
   one id per entry with no chunking and no `log_changes` inside the loop, and both callers -
   `catalog/pulls.rs:140`, and the rescan at `catalog/scan/mod.rs:333` whenever a revision's
   card changes - then drain it into one `SearchChange` insert per id in that same
   transaction. For a revision of N entries that is N primary keys materialised by the
   secondary scan, N whole `Entry` rows decoded to read one field, N times eight bytes held,
   and N row inserts, all before one commit. **Closes when** a fixture with one large revision
   has measured it and the result is either a fix with a before and an after, or a recorded
   verdict that revisions do not get large enough to matter.

5. **Two files over the 1,500-line cap.** `crates/hoard-index/src/catalog/search.rs` at 4,062
   lines and `crates/hoard-index/src/catalog/tests.rs` at 11,603. **Closes when** both are
   split the way `scan.rs` was, by responsibility, with no sibling import changed and no test
   count moved.

Items 2, 3 and 4 are all about the same question and share a harness run; do not seed three
separate catalogs for them. Item 5 needs no measurement and blocks nothing, so it goes last.
