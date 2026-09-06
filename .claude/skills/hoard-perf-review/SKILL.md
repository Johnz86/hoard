---
name: hoard-perf-review
description: Review hoard for what will not hold at a million artifacts - read-only, ranked findings with exact path:function, cost and evidence, plus the checklist of what to look for and a list of what was checked and found fine.
allowed-tools: Read, Grep, Glob, Bash
---

You are **read-only**. Change nothing: no edits, no commits, no `cargo fmt`, no fixes however
small. Finish by confirming `git status --short` is exactly what it was when you started.

Read `.claude/skills/hoard-catalog/SKILL.md` first; most findings here turn on a native_db or
Tantivy semantic that is not visible in the calling code. When you need a number rather than
a reading of the code, follow `.claude/skills/hoard-measure/SKILL.md`, including the
like-for-like rule: rebuild the previous commit in a scratch clone and measure both in one
session.

## The checklist

Work through all of it, not only the part the change touched.

1. **Full-table scans on a request path.** A route that reads every row of a table to answer
   one question. Grep for `.scan()`, `.all()`, `.collect::<Vec<_>>()` reachable from
   `crates/hoard-server/src/api/` and `crates/hoard-server/src/hub/`.
2. **Per-request loads of a whole table** into memory before keeping a page of it. A
   `limit` applied after the collect rather than before.
3. **Secondary-key scans used as if they were lazy.** A secondary scan materialises every
   matching primary key and decodes every yielded row; `take(n)` on one saves nothing.
4. **Transaction and write patterns.** Unbounded transactions, `upsert` where `insert` would
   do, one transaction per row where one per batch would do, a commit inside a loop. Every
   commit fsyncs.
5. **The search index and the collectors.** Anything retained per matching document rather
   than per page; a facet pass that keeps candidates; a second whole collected set held
   beside the first; a term spelled per hit rather than per ordinal per segment; deep offsets
   that walk from zero.
6. **Hashing I/O.** A file read once per digest rather than once with both hashers fed from
   one buffer; a pass that waits for the slowest file before committing any of them.
7. **Memory held for a whole scan.** Anything accumulated across units rather than per unit;
   a map keyed by a path string rather than by its hash; rows collected before being written.
8. **Concurrency.** Blocking catalog work off `spawn_blocking`; a lock held across IO; a
   `Mutex` where an `RwLock` or a channel belongs; the reactor blocked.
9. **Counts that drift.** Every writer of an entry must move the derived counts through
   `DerivedShift` and log its artifacts through `log_changes`. A writer that does not is a
   correctness finding, not only a performance one; `hoard doctor`'s summary check is the
   proof.

## The report

Findings ranked, most severe first, in three severities:

- **blocks scale** - it will not hold at 1M and the threshold it misses.
- **measurable** - it costs a measurable amount at a size hoard reaches.
- **minor** - real but below the noise of the measurements.

Each finding, in this shape:

```
[blocks scale] crates/hoard-index/src/catalog/read.rs::sources
  Cost: reads every Source row and every Entry beneath it; O(entries) per request,
        about 260 ms at 100k entries.
  Evidence: read.rs:173 scans primary and then read.rs:198 walks repositories_of_source.
  Fix: keep the count on the Source record and move it from DerivedShift, so the route
       reads a number off a row it already has.
```

Then, in the same report:

- **Checked and found fine**: the parts of the checklist you worked through that hold up,
  one line each, so the next reviewer knows what not to repeat.
- **The single change to make first**: one finding, named, with why it is first.
- **Confirmation that nothing changed**: the output of `git status --short`, and if you ran a
  server, that `pgrep` found none left.

Do not propose a fix longer than three sentences. If a fix needs a design, say so and stop.
