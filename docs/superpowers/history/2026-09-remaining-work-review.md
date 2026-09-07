# The remaining-work run, reviewed

Written after wave 8 and the two whole-delta reviews, as
`docs/superpowers/specs/2026-09-06-remaining-work-goal.md` asked. The run started at
`3a9fa1f` on 2026-09-06 and ended at `ae0265e` on 2026-09-07 with thirteen commits on
`master`, each a wave that ran in its own worktree, was verified, reviewed and squashed.
Nothing has been pushed.

## What each wave changed

| commit | wave | what landed |
| --- | --- | --- |
| `0ad5a49` | goal | the brief, the finish condition, the one-measurement rule |
| `fd862a2` | 1 doctor-and-sources | doctor diagnoses through an open path that writes no row and repairs only under `--repair`; `hoard catalog compact`; sources list every stored row with `can_scan` and `managed_by`; the owner's six decisions recorded; Phase 11's storage replacement closed |
| `f4627d6` | 2 scan-coordination | one lane for every catalog mutation under serve with duplicate requests merged and the queue drained on shutdown; a rebuild no longer prunes changes logged while it ran; the two writers of the search directory serialised |
| `506798d` | 3 live-rescan | the watcher started by serve, submitting to the lane with the configured policy; overflow, a full queue or a kernel refusal forces a reconciliation; Hoard's own directories excluded however spelled; `/healthz` reports the watch; Phase 10 closed |
| `a350757` | 4 query-bounds | the collapse path folds on the fly inside the collectors, with each artifact written as one operation group so the fold's run invariant holds; the count pass removed; the base rail capped at 32 with family roots kept |
| `1a3d944` | 5a scan-correctness | revision ordinals stable across reordering and an advancing head; names by the unit holding the most content in either discovery order; byte-exact path identities; unchanged revisions open no documents; a dropped source deleted by the next full rescan |
| `bb3d13b` | 5b readers-and-usage | the triage queue without an index; a maintained usage record and a keyed window; the eviction sweep in chunks; the apply job settled on every exit; a clone's remote recorded; forwarded clients parsed as addresses |
| `ec34057` | 6 recovery | a kill harness for every stage; a killed fresh scan rescanned; derived counts and the artifact sweep moved into every transaction; a pass mark that recounts once after a kill; unpublished search directories removed on open |
| `3223049` | 6b rescan-identity | a stored path known by its location; an hf-cache file rewritten in place replaced rather than colliding; a query between a rescan and its refresh stale rather than failed, with the cursor still advancing |
| `f36f986` | 7 disk | one card per revision; every record on the compact codec; sixteen spread tensor samples; the page-packing design |
| `7bb681f` | 7b hf-cache-cards | hf-cache files classified by their snapshot name, so cached READMEs become cards and configs reach inference; the dead sniff version deleted |
| `7a57762` | 8 final-measurement | the tables below |
| `ae0265e` | 9 fix-wave | shutdown interrupts a hashing pass within a megabyte; a deep window is counted before it is selected; the two slower routes shown not slower, paired and warmed, and the Hub metadata routes' discarded read removed; the closing reviews' findings closed; the rescan's maps, the sweep's writes and the usage chart bounded per pass; a revision the refile emptied retired to a tombstone |

## The numbers

Wave 8 built `3a9fa1f` and `7bb681f` in one session and measured both on one tree.

**The standing gates at 1M, at `7bb681f`**

| gate | measured | verdict |
| --- | --- | --- |
| allocated disk at rest | 1,397,235,712 bytes | met, by 102.8 MB |
| allocated disk after the incremental workload | 1,400,991,744 bytes | met, by 99.0 MB |
| allocated disk at the rebuild peak | 1,461,485,568 bytes | met, by 38.5 MB |
| peak scan RSS growth | 908,177,408 bytes | met, by 1.1 GB |
| rebuild no slower than the scan | 7.995 s against 76.954 s | met |
| every primary query under 500 ms | slowest 72.995 ms (midpoint) | met |
| the collapse queries under 500 ms | 50.887, 107.071, 74.765 ms | met |
| every page of the default order under 500 ms | 21.5, 73.0, 38.8, 76.7 ms | met |
| no route slower than the column beside it | three routes +1.76, +1.87, +0.82 ms | missed |

**1M, `3a9fa1f` against `7bb681f`, first run of two**

| measure | `3a9fa1f` | `7bb681f` |
| --- | --- | --- |
| scan wall | 74.838 s | 76.954 s |
| rebuild wall | 7.459 s | 7.995 s |
| allocated disk after rebuild | 1,399,160,832 | 1,397,235,712 |
| first page | 20.941 ms | 21.524 ms |
| last page | 21.337 ms | 38.807 ms |
| middle page | 71.924 ms | 72.995 ms |
| middle page by size | 58.185 ms | 59.376 ms |
| `role:lora` with facets | 24.028 ms | 24.759 ms |
| filtered | 38.206 ms | 39.585 ms |
| `node:self` with facets | 198.184 ms | 50.887 ms |
| `node:self` at the midpoint | not in the harness | 107.071 ms |
| `node:self` at the deep offset | not in the harness | 74.765 ms |
| middle page after a refresh | 76.582 ms | 76.386 ms |
| rescan of 1,000 changed | 5.237 s | 5.068 s |

The collapse query is the result of the run: 74 per cent off, on spreads of 1.9 and 0.2 ms.
The last page is a reproducible regression of 17.5 ms, the price of dropping the count pass
in wave 4, twelve times inside the gate. Five rows are 1.5 to 3.6 per cent slower with no
mechanism, within 0.15 ms at 100k, so it does not scale.

**The eleven routes at 100k, median of six over two server starts**: `node:self` 22.20 to
11.70 ms and the triage queue 4.64 to 3.45 ms improved. Three are slower beyond their
spread: the model detail route 2.79 to 4.55 ms, the default search page 3.71 to 5.58 ms,
the usage route 1.50 to 2.32 ms. The usage change is attributed (wave 5b trades one
whole-log read for a window scan, a newest-pulls scan and a record lookup, which is more
work on a small log and buys the asymptote); the other two are not.

**The supplemental fixture with cards, 30,000 entries**: whole directory 134,848,512 to
118,280,192 bytes, `SearchText` stored 17,335,378 to 5,568,950, one card per revision at
287,597 bytes over 600 rows.

**The workstation**: a fresh scan of the three configured sources in 3.465 s with hashing
off, 103 repositories, 230 artifacts, 728 entries, format 6; `hoard doctor` passes every
check and `--repair` reports no drift; `serve` answered `/healthz` 31 ms after start with
all three sources watched on 439 inotify descriptors and exited 15 ms after SIGTERM. One
earlier serve, whose startup reconciliation was hashing under the background policy, took
about 4.5 minutes to exit because shutdown waits for the lane.

## What the reviewers found on the whole delta

Every wave was reviewed before it landed, by `rust-reviewer` for correctness waves and
`perf-reviewer` for query waves, and every finding above minor was fixed on the wave's
branch before the squash. The closing reviews looked across waves.

**The Rust review of the whole delta** found one transient defect and a set of
contradictions, all closed by wave 9: a rescan could refile a row from an hf-cache snapshot
still on disk onto a newer snapshot sharing its blobs (wave 6b's location key against wave
5a's refile); the recount used the default triage threshold while the scan used the
configured one, a pre-existing gap that wave 1's doctor and wave 6's post-kill recount both
built on; a retired repository kept its usage rows; projection roots were not excluded from
the watcher; the API document still said a source row is never removed and the catalog
document still said there is no compact subcommand; the module maps lacked the two new
records; and the handoff's follow-ups had grown into a measurement narrative. It confirmed
the coordinator lane and the watcher hold against the recovery pass mark and the
location-keyed known map, the collapse fold holds against the one-card-per-revision
documents, every public function added has a caller, every format bump is a bump and
nothing else, every commit carries both trailers, and the check suite at the head is 862
passed, 0 failed, 4 ignored.

**The performance review of the whole delta**: it read every function on the two slower routes' paths at both commits and found
neither route's code changed nor reached code that changed; every candidate mechanism was
ruled out with evidence, and what is left is that these two routes make by far the most
random primary point reads per request (about 2,000 and 400) into a file wave 7 rewrote a
million bytes smaller in a different page order. It ranked twelve findings, the first being
the shutdown that joins an unbounded hashing pass; the rest were the rescan's three resident
maps at about 290 MB per million entries, the eviction sweep's unchunked retire write and
per-repository collect, the usage chart still decoding a week of rows and its recent-pulls
scan materialising every key, a revision no ref reaches never being retired and its card
orphaned with it, the Hub metadata routes reading an artifact record per sibling for a flag
they discard, the watcher's inotify walk on the runtime before the listener is served, a
pre-existing inclusive-end range on a primary key, and a named mechanism for the five 1M
rows 1.5 to 3.6 per cent slower: the per-document path match wave 4 added to every
primary collector. It confirmed no request path scans a table, every secondary scan on the
routes is bounded, the write path commits per unit with the derived counts inside, the
card read on the build path is one point read per revision run, the client list is bounded,
and the query path holds no lock. Wave 9 took all of it: the shutdown, the deep window, the resident maps, the sweep's writes, the usage
chart on the record, the tombstoned revision, the watcher off the runtime thread, the range
end, and the Hub routes' discarded read all landed in wave 9, and the two slower routes
turned out not to be slower at all: paired, warmed and interleaved against the run's start,
with `/healthz` as a control inside 0.05 ms, the head is faster on both, and the wave 8
protocol of three unwarmed runs had about a millisecond of drift, more than the effect it
reported. The five slightly slower 1M rows came back inside one per cent of the start
without the collector change, which stays recorded as the first thing to try if they slow
again.

## Recommendations

**On the finish condition.** Every line of the goal's finish condition holds at `ae0265e`. Waves 1 to 8 and the
fix wave are on `master`, one commit each, with 5 as two and three unplanned waves (6b,
7b, 9) that the run's own reviews and measurement produced, each verified and reviewed
before landing. Phases 10 and 11 are done with their acceptance stated. The handoff's
follow-ups hold the deliberately-not-code items and what wave 9 explicitly deferred. Doctor
passes on the workstation catalog and `--repair` reports no drift. Every standing gate at 1M
is met, and the three route rows wave 8 could not accept are met after wave 9's paired
re-measurement. The two closing reviews' findings above minor are fixed; the minor ones are
recorded. This document is the review, and the handoff points at it.

**For the first release.** The catalog is one format, rebuilt on any mismatch, with no
migration; that policy holds until the release and is decided in `CLAUDE.md`
"Compatibility" and nowhere else. Before shipping, decide it once: either the first
released format is version N and every later change ships a converter, or the release
keeps the rebuild-on-mismatch rule and says so. The recovery harness, the pass mark and the
doctor's repair path make the rebuild rule safe to keep for a first release. Three things
should be measured on a real cache before shipping rather than on the synthetic tree: the
base rail at hundreds of distinct bases, the rebuild's buffered peak with multi-entry
artifacts and cards (the writer pipeline is bounded in artifacts now), and the watcher's
startup walk against the kernel's watch limit on the largest source.

**The design-only items**, in the order the numbers rank them:

1. Dense page packing. Intra-page slack is 45 per cent of the redb file and compaction does
   not touch it. `docs/superpowers/research/2026-09-page-packing.md` says what would: a
   bulk load in key order or an append-biased split, both of which mean patching redb, since
   its page size is settable only under test configuration. The half-day experiment in its
   section 8 is the next step, and only if the disk margin ever tightens.
2. The verbatim GGUF metadata in `ArtifactDetail` is stored and never read after the write.
   Dropping it is the same kind of saving as the sample bound and the next largest bite out
   of the largest table on a real tree, at the cost of emptying a field of a public view.
3. TREE and DISK views have no design. Phase 10 closed without them; they need the
   proposal's design before any code.
4. A non-UTF-8 path is identified but cannot be stored, and a vanished revision's slot is
   never reclaimed. Both are record-shape decisions with the same answer as pruning a
   deleted path's row, which this workspace deliberately does not do.

**What the loop should change.** Three things cost this run time that a rule would have
saved. First, every plan I wrote stated facts the implementer then found wrong at a rate
of one or two per wave (a run invariant that did not hold, a fixture that could not be
extended, a hook that did not exist); the plan skill should require the dispatcher to run
the plan's own gate greps and, for any claimed invariant, to name the code that enforces it.
Second, reviewers found regressions against earlier waves twice (wave 6b against 5a, wave 7
against 5a); a wave's plan should list the tests of the waves it builds on as pins, and the
reviewer brief should name those waves. Third, the session limit interrupted three agents
mid-wave; the worktree flow lost nothing, but the dispatcher's scratchpad drafts were lost
once, so drafts that must survive belong in the worktree beside the plan, not in the
session scratchpad. One rule earned its keep: measuring once at the end, with a before
column rebuilt in the same session, gave a clean table for eleven waves at the cost of one
fix wave, where per-wave measurement would have cost eleven.
