# Goal: finish the remaining work in waves

Set on 2026-09-06 at `3a9fa1f`. This document is the brief for every wave that follows,
the finish condition that ends the run, and the rule that performance is measured once, at
the end. `docs/handoff.md` points here; the decisions it applies are the owner's decisions
of 2026-09-06, reproduced in the first wave's plan and recorded in the handoff by it.

## What "remaining work" means

Everything open at `3a9fa1f` that is code rather than a design still to be made:

- the owner's decisions 1, 2, 3, 4 and 6 (doctor, compaction on request, live rescanning,
  source actions, phase descriptions);
- the query follow-ups: the unbounded collapse path, the double count of a primary query,
  the uncapped per-bucket count of the base facet;
- the catalog correctness items under "Remaining work" in `docs/native-catalog.md`:
  `triage_queue` needing an index, revision ordinals recomputed on rescan, unit counting
  when another revision claimed the content, complete git metadata with stable cross-scan
  identities, byte-exact path identities, a rescan re-reading unchanged documents, the
  USAGE tab reading a repository's whole log, the hourly eviction sweep collecting the
  whole mirror;
- recovery: process-kill and power-loss tests, and whatever they find;
- disk: one card per revision instead of one per artifact, the remaining records on the
  compact codec, and a smaller `ArtifactDetail` summary.

Deliberately not code in this run, and therefore not in the finish condition: dense page
packing (a storage-engine investigation, delivered as a written design), the TREE and DISK
views (no design), a read-only redb open (redb does not offer one), the `native_model`
unwrap on a truncated row (upstream), `sweep_artifact` decompressing a row to delete it
(native_db has no remove-by-key), scheduled server compaction (measure churn first).

## The waves

Each wave is one plan, one worktree, one fresh implementer, verification, one review, one
squashed commit on `master`, in the order below. Every plan carries the standing gate,
conventions and contract fence from `.claude/skills/hoard-write-plan/SKILL.md`, plus the
measurement rule of this document: **no wave before the last runs the scale harness or the
route list**. A wave that changes a measured path states in its report which rows it
expects to move and why; the last wave measures them all.

1. **doctor-and-sources.** Decisions 1, 2 (the `hoard catalog compact` command), 4 and 6,
   the two softened claims, and the handoff's decision record. **Landed as `fd862a2`.**
2. **scan-coordination.** One coordinator for every producer of catalog mutation in
   `serve`: manual scans, API scans, hashing, refresh and rebuild run on one lane, requests
   for the same source coalesce, and a rebuild never overlaps a writer. The precondition
   decision 3 names before the watcher. **Landed as `f4627d6`.**
3. **live-rescan.** Decision 3: the watcher started by `serve`, enabled by default for
   configured filesystem sources with an opt-out; watched scans go through the coordinator
   with the configured hash policy; overflow or event loss marks the source dirty and
   schedules reconciliation; roots that overlap Hoard's own catalog and cache are excluded;
   periodic reconciliation with a configurable interval; the watcher is joined on orderly
   shutdown; an unavailable watch is a visible degraded status while manual scans keep
   working. Phase 10's watcher criterion is met when this lands. **Landed as `506798d`.**
4. **query-bounds.** The collapse path bounded the way the primary path is, reusing the
   sampled selection and the per-segment tallies; the primary query counting its match once;
   the base facet rail capped to the buckets the UI renders before the per-bucket count
   (the default taken for the open decision, see "Decisions still open").
   **Landed as `a350757`.**
5. **catalog-correctness, in two commits.** The correctness items listed above, each with
   a test that fails at the commit its wave starts from. **5a, the scan half:** revision
   ordinals stable across rescans, consistent unit counting, byte-exact path identities, a
   rescan skipping document re-reads when every stamp is unchanged, and a source dropped
   from the configuration retired by the next full rescan. **5b, the readers:**
   `triage_queue` answering from the catalog when no index exists or every catalog carrying
   one, git metadata and stable identities, a maintained client count for the USAGE tab, and
   the eviction sweep chunked. **Landed as `1a3d944` and `bb3d13b`.**
6. **recovery.** Process-kill and power-loss tests for the catalog and the search
   directory: a scan, a hash pass, a refresh and a publish each killed at a chosen point
   reopen to a consistent catalog and complete on the next run to the same counts as an
   uninterrupted run. Fixes for what the tests find land in the same wave.
   **Landed as `ec34057`.**
6b. **rescan-identity.** The two defects wave 6's hf-cache fixture found at its gate and
   left, which were not a kill's doing: a rescan keyed its map of stored rows by the
   absolute path their bytes were last found at rather than by the location they are filed
   at, so an hf-cache file rewritten in place collided on the unique location key; and a
   query landing between a rescan and its refresh failed on an entry the published index
   still named rather than answering with `stale` set. **Landed as `3223049`.**
7. **disk.** One `SearchText` card per revision referenced by its artifacts, the remaining
   records (`SearchChange`, `Repository`, `Source`, `Node`, `Usage`, user data) on the
   compact codec, and the `ArtifactDetail` summary reduced to what the inspector shows.
   Plus, as a written design only, the dense page packing investigation.
   **Landed as `f36f986`.**
7b. **hf-cache naming.** The defect wave 7's gate found and left: an hf-cache file was
   sniffed by the blob its snapshot links to rather than by the name the snapshot gives it,
   so a link whose blob carries no extension was filed under the wrong kind.
   **Landed as `7bb681f`.**
8. **final-measurement.** The whole protocol of `.claude/skills/hoard-measure/SKILL.md`:
   the eleven routes at 100k, the scale harness at 100k and 1M with two 1M runs, the
   supplemental fixture, and a fresh scan of this workstation's real disks with `hoard
   doctor` after it. Every threshold stated as met or missed with the number, in
   `docs/native-catalog.md`, against the last recorded column (`e2e1809` in the tally wave's
   tables). A miss produces a fix wave and a re-measurement of the missed rows only.
   **Landed as `7a57762`.**
9. **fix-wave.** The three things wave 8 could not accept, and what the closing reviews
   found across the whole delta: a shutdown that no longer waits for a hashing pass, a deep
   window counted before it is sampled, the two routes that slowed attributed, and the
   ranked findings of both reviewers fixed or recorded. Then the missed rows, and only
   those, measured again. This is the fix wave the measurement rule above provides for.

After the last wave: `rust-reviewer` and `perf-reviewer` review the whole delta from
`3a9fa1f` to the head, their findings are fixed or recorded, and the dispatcher writes the
review and the recommendations (below).

## Finish condition

The run is finished when every line holds:

1. Waves 1 to 9 are on `master`, one commit each except wave 5, which is two, 5a and 5b,
   wave 6, which is two, 6 and 6b, and wave 7, which is two, 7 and 7b; each verified by
   `.claude/skills/hoard-verify/SKILL.md` and reviewed before landing.
2. `docs/feature-plan.md` marks Phase 10 and Phase 11 done, with the acceptance checks of
   each stated and passing.
3. `docs/handoff.md` "Follow-ups" holds only the items named above as deliberately not code
   in this run, and "Decisions" holds none that is open, other than those the owner defers
   explicitly.
4. `hoard doctor` passes every check on this workstation's catalog after the fresh scan of
   wave 8, and `hoard doctor --repair` reports no drift. **Held.** At `7bb681f`, over a
   catalog scanned fresh from the three configured sources into the session scratchpad, every
   check reads `ok` and `--repair` reports `no drift; the stored totals were already exact`.
5. Wave 8's tables show every standing gate met at 1M: allocated disk under 1,500,000,000
   bytes at rest and at the rebuild peak; every query on the list, the collapse queries and
   the midpoint pages included, under 500 ms; peak scan RSS growth under 2 GB; rebuild no
   slower than the scan; and no route of the eleven slower than the last recorded column by
   more than its own spread without a stated reason. **Held.** At `7bb681f`: allocated disk
   1,397,235,712 at rest and 1,461,485,568 at the rebuild peak; the slowest query of any kind
   107.071 ms, which is `node:self` in the middle of the order; peak scan RSS growth
   908,177,408 bytes; rebuild 7.995 s against a scan of 76.954 s. Three routes of the eleven
   read slower there than the `3a9fa1f` column measured beside them, which is what wave 9 was
   for. Re-measured paired and warm over one tree in one session, at the head: the model route
   is 0.99 ms faster than `3a9fa1f`, the default search page 0.10 ms faster and the usage
   route 0.45 ms faster, with the `/healthz` control flat; the difference between `3a9fa1f`
   and `7a57762` on the two unexplained routes reads +0.12 and +0.01 ms in one dataset and
   -1.19 and -1.99 ms in another, so it was the timing protocol and not the delta. The last
   page of a million is 21.453 ms against the 21.405 ms `3a9fa1f` gives beside it, and an
   orderly shutdown with a hashing pass in flight is answered in 0.040 s. The numbers are in
   `docs/native-catalog.md` under "The fix wave's rows".
6. The two reviewers report no finding ranked above minor that is not fixed or recorded
   with a reason.
7. The review and recommendations are written and the handoff points at them.

## Decisions still open

- **The base facet count.** Wave 4 takes the default of capping the rail to the buckets
  the UI renders before running the per-bucket count. The alternative, answering the prefix
  rule from ordinals, changes what a bucket means. The owner can overturn the default
  before wave 4 opens; after it lands, overturning is a wave of its own.
- **Dense page packing** is delivered as a design in wave 7, not a change. Whether to act
  on it is the owner's call after the final measurement says what the slack costs.

## Review and recommendations

Written: `docs/superpowers/history/2026-09-remaining-work-review.md`, at `ae0265e`. The
finish condition holds; the review says how each line holds and what comes next.

Written by the dispatcher after wave 8 and the two reviews, to
`docs/superpowers/history/2026-09-remaining-work-review.md`: what each wave changed, the
final table against the `3a9fa1f` column and the `95cd3ad` column before the disk wave, the
misses if any, what the reviewers found and what was done about it, and recommendations
for what comes after the finish condition: the first release, the design-only items, and
what the loop itself should change.
