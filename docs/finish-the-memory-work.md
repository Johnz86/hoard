# How to finish the memory work

Written 2026-09-07 for whoever picks this up. It is a completion plan, not a goal document.
Read it and act; do not write another spec.

## Where things stand

The owner's ranked list of 2026-09-07 named eleven items. Items 1, 2 (the interruptible
half), 4 and 8 were already implemented before any of this started. Items 9 and 10 are
deliberately deferred (upstream work, and a retention decision). **Items 2 (the scheduling
barrier), 3, 5, 6, 7 and 11 were built, reviewed, fixed, and then reverted unmeasured.**

`master` is the pre-run baseline plus three things: corrected process rules, a retrospective,
and a measurement harness that did not exist before.

**The work is not lost and does not need rewriting.** The tag
`reverted/2026-09-07-predictable-memory` holds all four waves *after* their review rounds:
every one of the nine defects review found was fixed before each wave landed. Restoring it is
about 3,300 lines across 18 files, and it passed 908 tests, clippy and fmt at the point it was
tagged. What it never had was a measurement. The harness on `master` now provides one.

## The one decision that makes this cheap

Do not rebuild. **Restore from the tag, measure it, and keep what earns its place.** Rewriting
four waves from scratch costs what it already cost. Restoring costs one merge.

## Steps

Each step has an exit condition. Do not start the next until the previous one meets it.

**1. Restore the code, keep the new harness.**

```sh
git checkout -b wave/restore reverted/2026-09-07-predictable-memory -- crates/ src/ hoard.example.toml
```

Then reapply the measurement harness from `master`, because the tag's `scale.rs` is the older
one: take `master`'s `crates/hoard-index/tests/scale.rs` wholesale, then port the tag's own
harness additions (the wide-card fan-out phase under `HOARD_SCALE_WIDE_CARD_ARTIFACTS`) into
it. `docs/native-catalog.md` on `master` is authoritative; do not take the tag's.

*Exit:* `cargo test --workspace` is green, `git diff` against the tag under `crates/*/src` is
empty.

**2. Measure it against `master`, once, in one session.**

Both columns in the same session per `.claude/skills/hoard-measure/SKILL.md`. `master` is the
before column and needs no scratch clone, because it is the checkout. The format version
differs (7 against 8) and the usage record changed shape, so **each column seeds its own
catalog in its own tree**; they cannot share a directory.

Run at 1M, twice per column: the gate rows, plus the three workloads `master` added
(`HOARD_SCALE_DELETE`, `HOARD_SCALE_CONCURRENT`, `HOARD_SCALE_USAGE`) and the tag's fan-out
shape. The baseline numbers to beat are in `docs/native-catalog.md` under "Three workloads the
harness had never run, at 1M".

*Exit:* one table, both columns, every row present.

**3. Keep, drop, or fix, per wave, on the numbers.**

This is the step the original run skipped. For each of the four waves, the table says one of
three things:

- **It beats the baseline outside the spread.** Keep it.
- **It matches the baseline.** Drop that wave's changes. Complexity with no measured return is
  not worth carrying, and two of them are already suspected: wave 2's byte budget cannot bind
  below roughly 13 KiB of documents per artifact, and wave 4's grouped walk is invisible on a
  loose-source tree.
- **It is worse.** Drop it and record why.

Expect to keep fewer than four. The two most likely to earn their place are wave 4's summary
fold, worth about 136 MB resident at 1M, and wave 3's usage rekey, which turns retiring a
repository's log from quadratic in its length into linear.

*Exit:* a written keep/drop decision per wave with the number behind it.

**4. Pay the code-quality debt on what survives, and only on what survives.**

Mechanical, and much smaller once step 3 has dropped waves:

- Delete every `//` comment, including in tests. `AGENTS.md` forbids them and the reverted
  code contains some because a dispatcher asked for them.
- Delete every `///` on a private function that narrates implementation. Keep only public API
  contracts.
- Move every constant to the top of its file with the others. Known offenders in the tag:
  `SECONDS_PER_DAY` in `catalog/usage.rs`, `DOCUMENT_HEAP_BYTES` and `FIELD_VALUE_BYTES` in
  `catalog/search.rs`, `REPO_GROUP` in `catalog/scan.rs`.
- Split any file the surviving code pushes past 1,500 lines rather than growing it.

*Exit:* clippy, fmt and the full suite green; the greps above return nothing.

**5. Land it as one commit and update `docs/handoff.md`.**

## The open question that is not code

Eight concurrent midpoint queries at 1M take 632 and 681 ms for the slowest, against 72 ms for
the same page run alone, on a twenty-core machine. They run close to serialized. The standing
gate says every query is under 500 ms, and the single-request row is well inside it.

**The owner decides whether that gate binds under concurrency.** If it does not, there is
nothing to do and the gate should say "one query at a time" so nobody trips over this again.
If it does, the next step is a diagnosis that finds where the time goes and proposes nothing:
the likely suspects, in order, are the row-assembly reads through redb's page cache, the
searcher's own allocation, and the `search_cache` read guard. **Do not start that until the
gate question is answered**, because if the answer is "one at a time" the work is wasted.

## What not to do

- **Do not write another goal document.** This file is the plan.
- **Do not rebuild any of the four waves from scratch.** The tag has them, fixed.
- **Do not add more measurement workloads.** The harness now covers deletion, concurrency,
  usage, mixed-size hashing and the fan-out shape. That is enough to decide.
- **Do not run a review round on restored code.** It was reviewed twice per wave and every
  finding above minor was closed. Review only what step 4 changes.
