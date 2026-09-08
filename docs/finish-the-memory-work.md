# Memory work: delivered scope and remaining limits

Continuous hashing and metered index commits are delivered with repeated measurements,
correctness tests, and passing scale/concurrency gates. Read the current results in
[the native catalog report](native-catalog.md).

The next focused slice adds transaction-scoped summary folding, verified by rollback,
repeated-touch, snapshot-lifetime, and interrupted-recovery tests. It has no new scale
measurements; the previous performance evidence belongs to `51418ad`.

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
