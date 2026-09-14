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

Five items are open. Each closes by being fixed or by being measured and recorded as not
worth fixing; none closes by being deferred again. Ordered by whether something is currently
failing, then by what blocks what.

1. **The concurrency gate, the only one failing.** Eight simultaneous midpoint queries
   measured 632 and 681 ms against 72 ms for one, on a twenty-core machine, crossing the
   500 ms gate. No lock is held across the query path: `search_lane` is taken only by
   `refresh_search` and `rebuild_search`, and `open_index` returns a read guard. The
   discriminating measurement is latency against thread count at 1, 2, 4 and 8, taken at both
   100k and 1M: a curve that rises linearly with the thread count at both scales is
   serialization, and one that is flat at 100k and rises at 1M is memory bandwidth.
   **Closes when** `docs/native-catalog.md` carries that curve and the gate line above either
   reads as met or is restated with the reason it does not bind.

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

4. **Pending usage batches.** `counters.pending` drains inside `log_changes` once per write
   transaction, so it appears already bounded by `CHUNK` rather than by the pass. **Closes
   when** that is confirmed against the code and the item is struck from the limits above, or a
   path that skips the drain is found and fixed.

5. **Two files over the 1,500-line cap.** `crates/hoard-index/src/catalog/search.rs` at 4,062
   lines and `crates/hoard-index/src/catalog/tests.rs` at 11,603. **Closes when** both are
   split the way `scan.rs` was, by responsibility, with no sibling import changed and no test
   count moved.

Items 2, 3 and 4 are all about the same question and share a harness run; do not seed three
separate catalogs for them. Item 5 needs no measurement and blocks nothing, so it goes last.
