# Handoff

The state of the repository, how work is run on it, and what is left. Read `CLAUDE.md`
first; this document assumes it.

## Latest work: the memory goal is closed

The goal was that a rescan holds one repository at a time rather than the whole pass. It
landed as `b10c30d` (a standalone vehicle to measure a rescan's absolute peak, not a
high-water-mark difference), `9dd3eb8` (discovery reads and stores one repository window at a
time instead of a source whole) and `5e06f56` (a rescan loads and retracts one repository's
stamps, revision sizes and seen set at a time), followed by two module splits that touched no
production code: the `search.rs` split at `4c32a46` and the `catalog/tests.rs` split at
`568ec5f`.

At 1M files, one repository per folder, the rescan peak is 272 MiB (it was about 870 before
`9dd3eb8` and `5e06f56`). A single repository holding every file is still read and held whole
by design, and its rescan peak is 847 MiB.

Every open item in `docs/finish-the-memory-work.md` is closed; `568ec5f`'s commit message
says so directly.

**Next steps**, in this order:

1. Two defects the run against this workstation's real caches found, recorded with their
   evidence in `docs/native-catalog.md` under "A scan of this workstation's real caches":
   `hoard scan` on an empty data directory builds the catalog through `Catalog::open_or_scan`
   and then rescans it again in `src/cli/scan.rs::run`, reporting the second pass as
   "unchanged"; and a source's byte total sums every filed path's size, so hf-cache snapshot
   revisions that share a blob count it once per revision, 23.5 per cent over what the source
   holds on disk here. Whether a source's bytes should mean disk held is the owner's call before
   it is a fix.
2. Two correctness gaps neither closed goal touched: `native_model::Model::native_model_decode`
   panics on a stored row shorter than its 8-byte header instead of returning an error, and a
   retirement killed between `retire_entries` and `recount_retired_paths` leaves `path_count`
   stale in a way `hoard doctor` cannot see, because both the stored summary and a fresh count
   read the same stale value. Two more found while fixing the format rebuild: a run killed
   after `Catalog::create` makes the catalog directory but before `catalog.redb` exists leaves a
   directory that `open_or_scan` cannot recreate, so the next start fails on `create_dir`; and
   `hoard_mirror`'s `batch::tests::what_is_left_is_written_by_a_flush_and_by_a_shutdown` hung
   once while a release build and a rescan shared the CPU, and passed on a quiet rerun.
3. Splitting the files still over 1,500 lines: `crates/hoard-mirror/src/resolve.rs` (2,643),
   `crates/hoard-server/src/api/tests.rs` (2,426), `crates/hoard-net/src/peer.rs` (2,413) and
   `crates/hoard-index/tests/scale.rs` (1,837).
4. Only if the real run says memory still matters, sizing the redb read cache, `CACHE_BYTES` in
   `crates/hoard-index/src/catalog/mod.rs`, which is up to 230 MiB of the 272 MiB peak.

## Memory delivery, 2026-09-08 (commit `51418ad`)

Delivered continuous hashing and metered index commits, recovered selectively from
`fef969a` against immutable baseline `bdf2964`. Mixed-size hashing reaches its cancellation
checkpoint in 9.4 ms versus 570–574 ms; large-card rebuild memory is 350–382 MiB versus
744–772 MiB. Metering adds some rebuild overhead on small records. All final 100k/1M
measurements and the eight-request direct/HTTP gates pass; the selected HTTP maximum is
102.93 ms. See [the report](native-catalog.md#memory-verification-2026-09-08) for boundaries,
tradeoffs, exact source hashes, commands, and the single compressed evidence archive.

Final checks: 888 tests passed, workspace build, warning-free Clippy, formatting, actual
CLI/server smoke tests, cancellation/resume/hash equivalence, collapse/deletion refresh,
and interrupted publication/replay. The ignored measurement fixtures actually ran.

Format stays 7. No usage schema, event overload behavior, or source discovery change is
retained. Summary folding lacks isolated evidence and was removed. Usage rekeying and
writer/coalescing changes are deferred: format-8 recovery deletes and rescans rather than
preserving the existing catalog. Source-wide retirement bookkeeping, pending usage batches,
and oversized artifact groups remain unresolved; the full memory redesign is not complete.

All task-created worktrees, binaries, synthetic catalogs, and the external
`/home/johnz86/.cache/hoard-memory-evidence` directory have been removed. No experimental
branch remains and nothing was pushed. No user's catalog was used. Lost measurements from
the first interruption are excluded. Do not restart the restore-all-waves process; follow
[the remaining acceptance requirements](finish-the-memory-work.md).

## The repository

- The main checkout is `/home/johnz86/GIT/hoard`, on branch `master`; no experiment worktrees
  remain. **Nothing has been pushed.** The current memory task
  requires preserving populated format-7 catalogs; older compatibility assumptions do not
  waive that requirement.
- `docs/superpowers/specs/2026-09-06-remaining-work-goal.md` is the brief every wave now
  follows: what "remaining work" means, the finish condition that ends the run, and the rule
  that performance is measured once, in the last wave, rather than in each one.
  The run is finished: `docs/superpowers/history/2026-09-remaining-work-review.md` is the
  review of what each wave changed, the final numbers, what the closing reviews found and
  the recommendations for what comes next.
- `docs/finish-the-memory-work.md` records the delivered scope and remaining acceptance
  requirements as they stood when that document was written; the one-repository-at-a-time
  rescan goal it left open has since landed and closed, as "Latest work" above says.
- **The predictable-memory run of 2026-09-07 was reverted.** It set five waves and landed
  four, measured nothing, and was halted by the owner. Only the two components documented
  above have since been recovered. The original commits remain reachable at the tag
  `reverted/2026-09-07-predictable-memory`.
  `docs/superpowers/history/2026-09-07-predictable-memory-reverted.md` says why, what the
  process got wrong, and - the part worth keeping - the facts its reviews established about
  tantivy's per-document allocation, native_db's scan semantics, what the gate tree cannot
  show, and the hazards any future batching of a source's repositories has to respect. Read
  it before planning optimisation work on this catalog.
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
- Phases 0 to 11 are done. `docs/feature-plan.md` holds the table. The TREE and DISK views
  are not an unfinished phase: they wait on a design decision and are recorded under that
  document's "Risks and open decisions".
- Current memory-delivery checks are recorded above. Historical checks and measurements
  below describe their original commits, not a fresh verification of this delivery.
- Waves 1 to 9 of the goal are on `master` and the run is measured. The measurement is in
  `docs/native-catalog.md` under "Measured": the whole scale harness at 100k and 1M against
  a `3a9fa1f` column rebuilt and run in the same session over the same tree, the eleven
  routes at 100k in both columns, the supplemental fixture in both, and a fresh scan of this
  workstation's three sources with `doctor` and `doctor --repair` after it. Every standing
  gate at 1M is met. Wave 8's three route misses are settled under "The fix wave's rows":
  two of them were the timing protocol rather than the code, and all three read faster at the
  head than at the `3a9fa1f` column beside them.

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
   over. The plan carries the subject and body of the wave commit `master` will receive. Run
   every gate grep the plan names and quote the count each one found; for every invariant a
   task relies on, name the file and function that enforces it. Name the pinned tests: the
   tests of the waves this one builds on that must keep their assertions.
2. **Dispatch the implementer.** The `wave-implementer` agent, given the worktree path and
   the plan path, and nothing else. **Do not downgrade the model.** The reverted run of
   2026-09-07 ran every wave on `sonnet` to save tokens and spent far more of them on review
   rounds than it saved; the agent definitions carry the model and the dispatcher does not
   override it. It follows `.claude/skills/hoard-implement-plan/SKILL.md`:
   preflight in the worktree, the AGENTS.md rules, the checks, foreground-only process
   hygiene, a record of every deviation from the plan, one checkpoint commit per task, and a
   report. **A review finding is not automatically a task.** A small correction can be
   appended to the wave; anything larger means the plan was wrong, and a wrong plan is
   rewritten rather than patched by appending tasks to the agent that followed it. Appending
   round after round is what makes one transcript write something and then delete it, which
   is what the owner saw and stopped in the reverted run.

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
   read-only and review `master..HEAD` in the worktree. The reviewer's brief names the waves
   this change set builds on, not only `master`. Do not run a build while a reviewer is
   measuring, or dispatch a measuring reviewer beside anything else that uses the disk.
   Fixes from the review go on the same branch, as more checkpoints, before landing.
3b. **Halt and ask, do not push through.** Stop the run and put it to the owner, with the
   numbers, when any of these is true. Each one was true in the reverted run of 2026-09-07
   and none of them stopped it:

   - a review finds more than it found on the previous wave, rather than fewer;
   - a wave introduces a defect that did not exist before it, especially one that can lose
     data;
   - a wave lands with nothing measured, on a run whose purpose is a measurable property;
   - the goal document needs correcting for a second time.

   **Report the trend, not the events.** "Review found things and we fixed them" reads as a
   healthy loop and hides a rising defect rate. Say how many findings this wave drew against
   the last, and say plainly when a wave introduced a defect rather than only that it was
   closed.

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

Every measured number and the narrative that explains it lives in `docs/native-catalog.md`
under "Measured": the disk gate and where the bytes are, a page anywhere in the order, the
tallying pass, the collapse path, the routes at 100k, the fix wave's rows and the thresholds
table that answers each gate once. What is left here is what is deliberately not code, and
what wave 9 looked at and left.

- **Page packing is answered and not worth doing.**
  `docs/superpowers/research/2026-09-page-packing.md` reads redb 2.6.3: the 42 to 55 per cent
  leaf fill comes from one split rule that always cuts at the byte midpoint and has no append
  case, so insertion order, a bulk load in key order and a dense rewrite all land at the same
  fill, `Database::compact` moves whole pages and never rebuilds a leaf, and
  `Builder::set_page_size` is compiled only under `cfg(any(fuzzing, test))`. The reachable
  saving is 0 to 5 per cent; the lever that works is payload, at about two bytes of file per
  byte off a row. Whether to act on it is the owner's call, and the recommendation is not to.
- **The five 1M query rows that were 1.5 to 3.6 per cent slower are back, unexplained.** The
  first page, the two midpoints, `role:lora` and the filtered query read within 1 per cent of
  the `3a9fa1f` column at the head, inside the spread of two runs of the same binary. The
  hypothesis for the gap was codegen around the collectors' inner loop - wave 4 gave every
  primary collector a per-document `Collapsing::offer` that did not exist before - and it was
  never tested: monomorphising the four collectors over a `const COLLAPSING: bool` would
  duplicate every call site and the rows no longer ask for it. If they slow again, that is
  the first thing to try.
- **A revision slot is emptied but never reclaimed.** A revision no ref reaches and no entry
  names is written back as a tombstone and its card is dropped, so an advancing branch costs
  one empty slot per old head rather than a card and a commit per head. Reclaiming the slot
  itself needs the entries of a retired revision removed rather than marked absent, which is
  the same decision as pruning a deleted path's row; neither is taken.
- **A path outside UTF-8 is identified but not stored.** The identity a scan computes is
  byte-exact, but `Repository.root_path` and `Entry.placed` hold `to_string_lossy` text, the
  walker drops a relative path that is not UTF-8, and `SourceConfig` cannot serialise a root
  that is not. Storing the bytes is the remaining half, and it is a record shape change.
- **A read-only open** is waiting on redb offering one, so searches could run beside a
  rebuild rather than beside a lock.
- **A retirement killed before its recount leaves path counts stale with nothing to find
  them.** `retire_entries` marks entries absent in one transaction and `recount_retired_paths`
  corrects the affected artifacts' `path_count` from `Counters.touched` afterwards, which is
  an in-memory set. A kill between the two loses it, and because both the stored summary and
  a fresh count read whatever `path_count` is stored, the two agree and `hoard doctor` sees
  nothing. Found during wave 4 of the predictable-memory run and deliberately not fixed
  there: it is a different pass, and fixing it properly means giving retirement a durable
  record of what it still owes, which is a design rather than an edit.
- **Discovery now reads a bounded window of repositories, and a single repository is still
  read whole.** `9dd3eb8` made a scan or rescan plan a source's repositories and read, store
  and walk them a window at a time rather than discovering the whole source into memory
  first, which is what brought the per-folder rescan peak down from about 870 MiB to 272 MiB
  at 1M files. A repository of a million files in one folder is still held whole by design,
  and that tree's rescan peak stayed at 847 MiB.
- The narrower items each carry their own paragraph in `docs/native-catalog.md` under
  "Remaining work": the tally's slot per term of a column, the sampled path's fallback
  recollecting, `sweep_artifact` decompressing a cold row to delete it, and
  `native_model::Model::native_model_decode` unwrapping a payload shorter than its header.

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
  enforced. Every one of them is met. `hoard_index::coordinate::Coordinator` is the one lane
  every scan under `serve` runs on, `POST /api/v1/scan` submits to it and `serve` drains it,
  and `Catalog::search_lane` serialises `rebuild_search` against `refresh_search` while a
  rebuild prunes only what it indexed. `crates/hoard-index/src/watch.rs` is now a producer
  for that lane: `hoard_server::start_watcher` starts it from `[index] watch`, each debounced
  burst is one `ScanRequest` with `Origin::Watched` under the configured hash policy, an
  overflow, a lost signal or a watch the platform refused reconciles every source with
  `Origin::Reconcile`, a timer does the same every `reconcile_interval_secs` and once at
  startup, the data directory, the index directory and the mirror cache root are excluded
  whole and resolved to absolute paths before the debounce so a root spelled relative to the
  working directory cannot feed a scan its own writes, `/healthz` carries `watch` and
  degrades only on a watch the platform refused, and `hoard_server::serve` joins the watcher
  before draining the lane. A configured root that is not a directory is reported in
  `WatchStatus.missing` and is not degraded, because a scan skips it with a warning too.
  `Watcher::start` walks every directory of every source synchronously to register inotify
  watches, and each directory costs one watch against `fs.inotify.max_user_watches`.
  **Done in wave 3.**
- **Managed sources stay visible.** `GET /api/v1/sources` lists every source the catalog
  holds, including a peer's namespace, and each row carries `can_scan`, true exactly when the
  configuration names it, and `managed_by`, one of `"downloads"`, `"pulls"`, `"peer"` or
  `"nobody"` for a stored source the configuration no longer names. The UI offers SCAN on
  `can_scan` alone and labels the rest; no source name appears in JavaScript, and the backend
  still answers 404 for a scan of a source it does not walk.
  **Done in this wave.**
- **A source dropped from the configuration is deleted by the next full rescan.** Taken by
  default in wave 5a, as the question wave 1 raised. `Catalog::rescan` now takes a
  `RescanScope`: `Configured` says the config it was handed is the whole configuration, so a
  stored source row outside it has been dropped; `Narrowed` says the caller chose a subset
  and retires nothing. A dropped source's entries, repositories and the artifacts that lose
  their last path are deleted, moving the derived counts through `DerivedShift` and logging
  each artifact through `log_changes`. They are deleted rather than left as `Remote` rows
  because a path of a dropped source is not a path this node stopped holding, it is a path
  this catalog stopped being about: a repository row whose `Source` is gone is rendered by
  `repositories()` under a default kind, is still counted by `Summary.repositories`, and
  files every path a second time if the source is ever re-added. The mirror, pull and peer
  source rows are never a scan's to retire, and a source whose root is merely unmounted
  stays, because the configuration still names it. The effect on "Managed sources stay
  visible" is that `managed_by: "nobody"` is now the state of a dropped source between the
  drop and the next full rescan rather than a permanent one. **Taken in wave 5a.**
- **The triage queue stays local and actionable.** A peer-only artifact stays searchable as a
  remote artifact and does not enter this node's queue: local evidence decides local triage.
  **Already the behaviour; recorded, no code.**
- **Phase 1 stays complete and Phase 11 is done.** Phase 1's SQLite implementation is
  history, not an unfinished phase. Phase 10 is done as of wave 3, now that the watcher
  starts, reconciles and shuts down through `serve`; TREE and DISK need a design decision
  before either can be implemented and are a risk rather than a phase. Phase 11's storage
  replacement is complete; the optimisation that continues is a list of follow-ups with
  their own acceptance criteria under `### Phase 11`. **Done in this wave.**
- **How should the base facet be counted?** Taken by default in wave 4: the rail is
  truncated to the 32 most populous bases by tally before the per-bucket count runs, so the
  searches a query pays for the rail are bounded by a constant rather than by how many bases
  the catalog holds. The prefix rule is unchanged, so a bucket's count is still what a click
  on it answers; a base outside the 32 is not shown and is not marked active, and a query
  naming it still answers. `docs/api/hoard-api.md` says so. Answering the prefix rule from
  ordinals, which would change what a base bucket means, remains the other way out and is
  still not an implementer's call. **Taken in wave 4.**

Smaller things, each verified:

- A shutdown costs a megabyte, not a pass. The scan lane raises one stop when it closes: a
  walk checks it between units, a hashing pass before each batch and once per megabyte of
  every file it reads, and requests that had not started are cancelled. Both passes leave the
  pass mark set and retire nothing, so the next start settles the counts and finishes the
  hashing. On this workstation, SIGTERM during the startup reconciliation's hashing is
  answered in 0.040 s where it took about 4.5 minutes.
- A file listing reads the artifact behind each file only for the screen that renders the
  `deduped` mark. The Hub metadata routes, the projection, the peer digest and the cache
  sweep asked for a mark they threw away, at one random point read per file.
- The triage threshold a catalog was scanned with is stored in its manifest and every later
  writer counts against it, so `hoard doctor` no longer fails a healthy catalog scanned with
  a non-default threshold.

- The hourly cache eviction sweep walks the mirror in chunks. `hoard-mirror`'s
  `cache/reconcile.rs` calls `Catalog::served_cache_entries_in`, which reads the mirror's
  repositories through one prefix scan of the identity key and hands each repository's
  served rows over in chunks of 1,024, measuring each chunk against the cache tree as it
  arrives; the rows that are gone are retired once at the end. The residual is that one
  repository's served rows are materialised at a time, which is what a location prefix scan
  costs.
- An entry re-served between the sweep's walk and its retire is retired anyway, and the next
  serve of it files it again. Pre-existing: the walk and the retire were two catalog calls
  before this wave and still are, and chunking the walk did not widen the gap.
- A rescan recognises a stored row by where it is filed - repository, revision ordinal,
  relative path - rather than by the absolute path its bytes were last found at, so an
  hf-cache file whose content changes in place is replaced instead of colliding on the unique
  location key. A revision found at a fresh ordinal still refiles its unchanged rows onto it,
  from a second index by repository and path that leaves out a path more than one stored
  revision holds. `docs/native-catalog.md` under "The write path" says so.
- An hf-cache revision carries a card and a config. A snapshot links `README.md` and
  `config.json` at blobs named by their hashes, and the sniffer classified the blob's name,
  so every document of a cached revision was `Unknown` and its inference context was empty;
  no hf-cache revision had ever had a card. `sniff::sniff_named(path, name)` reads the header
  from the blob and classifies by the entry's own relative path, and `sniff_path` is that
  function with the path's own name. That wave bumped `FORMAT_VERSION`, because a blob's
  inode, mtime and size do not change when the rule that reads it does and a rescan would
  settle every stored revision on its stamps; the bump rebuilds every existing catalog from
  disk, which is what puts the cards into one. It is 7 now, moved once more by wave 9. Wave
  8's workstation scan therefore reads and indexes the READMEs and configs of every cached
  revision: its scan time, its card count and its search answers all move, and nothing in the
  synthetic tree does, because the tree has no hf-cache source.
- `sniff::SNIFF_VERSION` and `Artifact::sniff_version` are gone. The constant was written
  into every artifact record by two sites and read by nothing, and its doc comment promised a
  re-read of every header that no code performed. A per-artifact stamp could not have
  delivered it either: the documents whose classification changed are entry records with no
  artifact of their own. A sniffing or inference rule that changes is a `FORMAT_VERSION` bump
  and nothing else, which `docs/native-catalog.md` now says under "What the scan produces".
- A query that lands between a rescan and the refresh that follows it answers with `stale`
  set, skipping the rows that rescan replaced, instead of failing on an entry the published
  index still names. A skipped row counts as a position walked, so `next_cursor` is offered
  as it would have been and paging does not stop short. `docs/api/hoard-api.md` under
  "Search" says what a caller sees.

`docs/native-catalog.md` under "Remaining work" holds the rest: a truncated record panicking
inside `native_model` before any hoard codec runs, `sweep_artifact` decompressing a cold row
only to hand it back to `remove` because native_db 0.8.2 has no remove-by-key, storing a path
whose bytes are outside UTF-8 now that its identity is byte-exact, reclaiming the slot of a
revision the walk no longer finds, and a read-only redb open once one exists. What a kill at
each stage leaves and how the next run completes it is recorded under "Recovery" in the same
document, with the two writers whose totals a kill still leaves to `hoard doctor`.
`docs/feature-plan.md`
under `### Phase 11` names which of them carry acceptance criteria as follow-ups of that
phase.
