# Handoff

The state of the repository, how work is run on it, and what is left. Read `CLAUDE.md`
first; this document assumes it.

## The repository

- One checkout, `/home/johnz86/GIT/hoard`, on branch `master`. There is no worktree and no
  second clone. **Nothing has been pushed.** There is no remote work to reconcile with, and
  no release and no user - see the "Compatibility" section of `CLAUDE.md` for what that
  buys.
- `docs/superpowers/specs/2026-09-06-remaining-work-goal.md` is the brief every wave now
  follows: what "remaining work" means, the finish condition that ends the run, and the rule
  that performance is measured once, in the last wave, rather than in each one.
- The native_db (redb) plus Tantivy catalog replaced the SQLite index and is merged. Every
  command, every route, both faces and gossip read and write it, and no SQLite code remains.
  `docs/native-catalog.md` is the current-state document;
  `docs/superpowers/history/2026-09-native-catalog-waves.md` holds the condensed record of
  how it was built.
- Every catalog record has one shape at `native_model` version 1, and there are no
  migrations. `docs/superpowers/specs/2026-09-05-native-catalog-decision.md` and
  `docs/superpowers/research/2026-09-05-native-db-tantivy.md` are the decision and the
  dependency research; they stay. The wave plan files under `docs/superpowers/plans/` are
  deleted - git keeps them, and a stale plan in the tree is a trap for the next agent.
- Phases 0 to 9 and 11 are done; 10 is partial, waiting on the watcher through `serve` and on
  a design for TREE and DISK. `docs/feature-plan.md` holds the table.
- At the head of `master` after the scan-coordination wave all four checks pass:
  `cargo fmt --check`, clippy with `-D warnings`, `cargo test --workspace` at 803 passed,
  0 failed, 4 ignored, and `cargo build --workspace`. The four ignored are the scale harness
  and its supplemental fixture, the mDNS test that needs a multicast route and the sniffer
  test that reads the local model caches. `cargo doc --workspace --no-deps` emits no warning.

## Running the loop

Work runs as waves. Each wave is one worktree, one branch, one plan file, one fresh
implementer agent, a verification pass, a review, and **one squashed commit on `master`**.
The implementer's per-task commits are checkpoints on the branch and never reach `master`;
the plan file is never committed. The skills carry the contracts so no prompt has to repeat
them.

1. **Open the wave.** From the main checkout:
   `git worktree add ../hoard-<name> -b wave/<name>`. Then write the plan following
   `.claude/skills/hoard-write-plan/SKILL.md` to
   `../hoard-<name>/docs/superpowers/plans/<name>.md`, untracked. Anchor its Verified facts to
   the commit the branch starts from and grep every symbol it mentions before you hand it
   over. The plan carries the subject and body of the wave commit `master` will receive.
2. **Dispatch the implementer.** The `wave-implementer` agent, given the worktree path and
   the plan path, and nothing else. It follows
   `.claude/skills/hoard-implement-plan/SKILL.md`: preflight in the worktree, the AGENTS.md
   rules, the checks, foreground-only process hygiene, a record of every deviation from the
   plan, one checkpoint commit per task, and a report.

   The report is written so you can verify without reading the transcript, and section 7 of
   that skill fixes what it contains: the commit hashes and subjects in order, every
   deviation with what the plan said and what the code said, the final output line of each
   check quoted rather than "all checks pass", the measurement tables with a commit per
   column, the tests whose assertions changed and why, what was left for a human, and a
   clean `git status --short` with the `pgrep` result. A report missing a section is a
   report to send back.

   An implementer that stops early - a deviation that makes a task's outcome wrong, a check
   it cannot get green, a decision that is not its to take - stops after a checkpoint that
   builds and says so in its first line. Read its reasoning, take the decision yourself, and
   resume it with a message naming the next task; a fresh agent would re-derive the whole
   plan. An implementer that appears to be waiting on a long command is waiting on a
   foreground command, which is what the skill requires: let it finish, or raise the
   timeout and resume it. Never ask it to poll, and never start a background process on its
   behalf.
3. **Verify and review, in the worktree.** `/hoard-verify` there: `cargo fmt --check`, clippy
   with `-D warnings`, `cargo test --workspace`, `cargo build --workspace`, plus
   `cargo test --release -p hoard-server --lib assets::` after any change under
   `crates/hoard-server/` or `web/`, and `cargo doc --workspace --no-deps` once. Then
   `perf-reviewer` for a performance wave, following
   `.claude/skills/hoard-perf-review/SKILL.md`; `rust-reviewer` otherwise. Both are
   read-only and review `master..HEAD` in the worktree. Do not run a build while a reviewer
   is measuring, or dispatch a measuring reviewer beside anything else that uses the disk.
   Fixes from the review go on the same branch, as more checkpoints, before landing.
4. **Land the wave.** From the main checkout on `master`:
   `git merge --squash wave/<name>`, then one commit whose subject and body are the plan's
   wave commit, ending with the two attribution trailer lines this session was given. Then
   `git worktree remove --force ../hoard-<name> && git branch -D wave/<name>`; the force is
   for the untracked plan file, which goes with the worktree. Update this handoff inside the
   wave (a task of the plan) so it lands in the same commit, never as a commit of its own.

For anything touching `crates/hoard-index/src/catalog/`, read
`.claude/skills/hoard-catalog/SKILL.md` first: the module map, the write path, and the
native_db and Tantivy semantics that the calling code does not show. For anything measured,
`.claude/skills/hoard-measure/SKILL.md`, including its like-for-like rule - day-to-day
timings on this workstation drift by more than most of the effects being measured, so a
"before" number is rebuilt and measured in the same session, never quoted from a document.

`/hoard-next-phase` turns the next unchecked phase of `docs/feature-plan.md` into a wave
plan and hands it to the same loop.

## Follow-ups

Performance, with the numbers from `docs/native-catalog.md`:

- **The 1M disk gate is met by 100.4 MB.** Allocated disk after the rebuild is
  1,399,648,256 bytes against 1,500,000,000, a 41 per cent reduction from the computed
  location key, the compact and LZ4 record codecs, the trimmed search columns and no longer
  writing a change log before the first index is published. A rebuild that replaces a
  published index holds two search directories while it runs and peaks at 1,463,869,440
  bytes, inside the gate by 36.1 MB; that transient is the number to watch. The next levers,
  in order of measured size, are intra-page slack (608,835,673 bytes, 45 per cent of the redb
  file, which `compact` does not reclaim and only a dense rewrite would), the
  `ArtifactDetail` summary itself (497 MiB of pages, 37 per cent of the file), and one card
  per revision instead of one per artifact in `SearchText`. The per-table breakdown in
  `docs/native-catalog.md` is current.
- **A page anywhere in the order costs a page.** A window of at most 16,384 rows for a text
  key, or 131,072 for a numeric one, keeps the heap it always kept; a deeper one is found by
  rank, in three passes that sample the order, count the documents between consecutive
  samples and read only the bucket the rank falls in plus a page beyond it. Both constants
  are measured, in the crossover table under "Thresholds" in `docs/native-catalog.md`. At 1M
  the middle page of the default order is 184.5 ms against 1.508 s and holds 13.62 MiB above
  its own page against 58.56 MiB; the same page after a refresh, over four segments, is
  189.6 ms against 1.544 s. Two rows this wave did not put on a new path, `role:lora` and the
  filtered query, are 4.7 and 7.4 ms slower at 1M and outside their own spread; they are back
  on their before numbers at 100k, so it does not scale. Codegen around the collector's inner
  loop is the hypothesis for what is left, and profiling has not established it.
- **A tallying pass now costs about what a counting pass costs.** Each facet dimension is a
  vector indexed by term ordinal per segment rather than a `HashMap` keyed by
  `(segment, dimension, ordinal)` per document, and every collector reads its columns once
  per block of up to 64 documents through `first_vals` rather than once per document. At 1M
  the first page of the default order is 21.1 ms against 120.3 ms, the deep page 21.4 against
  120.8, `role:lora` with its rail 24.2 against 146.2, the filtered query 38.3 against 234.6,
  the middle page 72.0 against 184.5 and `node:self` 120.6 against 338.6. Of the 9.8 ms the
  first page lost at 100k, the vectors took 8.6 ms and the block reads 1.2.
- **A block read has to survive deletes to be worth anything.** tantivy's
  `default_collect_segment_impl` drops to one `collect` per document as soon as a segment
  carries a delete file, which every segment a refresh has touched does, so hoard's
  collectors override `Collector::collect_segment` and filter each block through the alive
  bitset themselves. Before that override the midpoint after a refresh sat 24.2 ms above the
  plain midpoint at 1M against 4.6 ms before the wave; with it, 4.7 ms. A new collector in
  this file must override `collect_segment` the same way or it silently falls back.
- **A primary query still counts its match twice.** `Selecting::of` runs a `Count` search so
  `Window::of` can say which slice of the order was asked for, and the tallying pass that
  follows counts the same documents again. That first pass is now about half of what a
  shallow primary query spends, and it is the largest cost left on the primary path. Removing
  it means the window deciding what it needs after the collection rather than before, which
  is a change to `Selecting` and `Window`, not to the collectors.
- **The collapse-path queries are under the 500 ms target without being bounded.**
  `node:self` is 352 to 439 ms at 1M; any query naming `node:`, `state:`, `source:`,
  `repo:`, `rev:` or `hash:`, or sorting on revision, node or state, takes the same path and
  still keeps one row per matching artifact rather than one per page. The primary path is
  bounded above the window caps and is the model to follow; below them the sampled selection
  falls back to the heap on purpose, so "paging is bounded" is a claim about the deep pages
  the caps cover, not about every page.
- **The usage views are O(rows of the repository).** The client count the USAGE tab shows is
  a claim about every pull the repository ever served, so bounding the chart alone would not
  bound the route: 20,000 rows cost 9.1 ms. It needs a maintained count, not a bound.

Decisions taken on 2026-09-06, by the project owner:

- **`hoard doctor` is diagnostic by default.** It reports drift and exits non-zero without
  correcting it; `hoard doctor --repair` recounts the derived totals, reports before and
  after and verifies the result. Counting is separated from storing: `Catalog::count_summary`
  reads one snapshot and writes nothing, `Catalog::repair_summary` is exclusive by signature.
  Doctor opens through `node::inspect`, which creates no directory, scans nothing, refreshes
  nothing and records nothing; a missing or foreign catalog is a finding it reports, not
  something it fixes on the way in. Read-only means no catalog row is written: native_db
  opens every table in a committed write transaction as it opens the file, so the file's
  transaction id advances whatever a caller does. **Done in this wave.**
- **Automatic rescans never compact.** `hoard catalog compact --directory <dir>` compacts on
  demand, takes the catalog to itself through the redb lock and reports allocated bytes
  before and after. A fresh scan still compacts at the end of building one, which is bulk
  construction. There is no scheduled server compaction until sustained churn is measured,
  and intra-page slack is not a promise of what a compaction returns: dense packing is a
  separate investigation. **The command is done in this wave; the rest is standing policy.**
- **Live rescanning ships, on by default for configured filesystem sources with an opt-out.**
  Its preconditions: manual and watched scans share coordination so neither runs over a
  source the other holds, the configured hash policy is preserved, an overflowed watch marks
  its source dirty rather than losing the change, Hoard's own writes under the data directory
  are excluded, shutdown joins the watcher, and the rebuild-versus-write boundary is
  enforced. **The coordination and the boundary are met:** `hoard_index::coordinate::Coordinator`
  is the one lane every scan under `serve` runs on, `POST /api/v1/scan` submits to it and
  `serve` drains it, and `Catalog::search_lane` serialises `rebuild_search` against
  `refresh_search` while a rebuild now prunes only what it indexed. `crates/hoard-index/src/watch.rs`
  is still started by nothing: the watcher wave makes it a producer of `ScanRequest`s with
  `Origin::Watched` and adds the dirty-source, hash-policy and shutdown-join behaviour.
- **Managed sources stay visible.** `GET /api/v1/sources` lists every source the catalog
  holds, including a peer's namespace, and each row carries `can_scan`, true exactly when the
  configuration names it, and `managed_by`, one of `"downloads"`, `"pulls"`, `"peer"` or
  `"nobody"` for a stored source the configuration no longer names. The UI offers SCAN on
  `can_scan` alone and labels the rest; no source name appears in JavaScript, and the backend
  still answers 404 for a scan of a source it does not walk.
  **Done in this wave.**
- **The triage queue stays local and actionable.** A peer-only artifact stays searchable as a
  remote artifact and does not enter this node's queue: local evidence decides local triage.
  **Already the behaviour; recorded, no code.**
- **Phase 1 stays complete and Phase 11 is done.** Phase 1's SQLite implementation is
  history, not an unfinished phase. Phase 10 stays partial until the watcher starts,
  reconciles and shuts down through `serve`, and TREE and DISK need a design decision before
  either can be implemented. Phase 11's storage replacement is complete; the optimisation
  that continues is a list of follow-ups with their own acceptance criteria under `### Phase
  11`. **Done in this wave.**
- **How should the base facet be counted?** Still open, and still not an implementer's call.
  `Catalog::facets` runs one `Count` search per distinct base value in the answer, uncapped,
  because the prefix rule that groups base labels means the exact-term count the tally already
  holds is not what a click on that bucket produces. `Base` is open ended, so the bucket count
  is bounded by what the catalog holds rather than by its size: at 200 distinct bases that is
  200 extra searches per request. Either the rail is truncated to the top N buckets by tally
  before the per-bucket count is run, which changes what the UI shows, or the prefix rule is
  answered from ordinals, which changes what a base bucket means.

Smaller things, each verified:

- The hourly cache eviction sweep is O(the whole mirror). `hoard-mirror`'s
  `cache/reconcile.rs` calls `Catalog::served_cache_entries`, which calls
  `read.rs::repositories_of_source_in` to walk **every** `Repository` row looking for the
  mirror source, then scans every entry of each and collects one `Vec<EntryDetail>` before
  anything is stat-ed. It is off the request path and the mirror source is small today;
  chunking it the way `scan.rs` chunks its passes is the fix if it ever is not.

`docs/native-catalog.md` under "Remaining work" holds the rest: a truncated record panicking
inside `native_model` before any hoard codec runs, `sweep_artifact` decompressing a cold row
only to hand it back to `remove` because native_db 0.8.2 has no remove-by-key, byte-exact
path identities,
`triage_queue` needing rather than tolerating a search index, skipping a rescan's document
re-read when every stamp is unchanged, revision ordinals recomputed on every rescan, counting
a revision's artifact units for content another revision already claimed, complete repository
git metadata with stable cross-scan identities, a read-only redb open once one exists, and
power-loss recovery tests. `docs/feature-plan.md` under `### Phase 11` names which of them
carry acceptance criteria as follow-ups of that phase.
