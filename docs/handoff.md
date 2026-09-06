# Handoff

The state of the repository, how work is run on it, and what is left. Read `CLAUDE.md`
first; this document assumes it.

## The repository

- One checkout, `/home/johnz86/GIT/hoard`, on branch `master`. There is no worktree and no
  second clone. **Nothing has been pushed.** There is no remote work to reconcile with, and
  no release and no user - see the "Compatibility" section of `CLAUDE.md` for what that
  buys.
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
- Phases 0 to 9 are done, 10 and 11 are partial. `docs/feature-plan.md` holds the table.
- At the head of `master` after the paging wave all four checks pass:
  `cargo fmt --check`, clippy with `-D warnings`, `cargo test --workspace` at 774 passed,
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
  on their before numbers at 100k, so it does not scale, and it is codegen around the
  collector's inner loop rather than any extra work.
- **The tally is the floor under every primary query.** Collecting a matched document costs
  about 105 ns against about 23 ns for a pass that only counts: six one-at-a-time
  string-ordinal reads, five `HashMap` entries and a `HashSet` insert per document, all
  SipHashing a three-part key. That is roughly 80 ms of the 121 ms floor at 1M and is paid
  again by each of the five narrowed facet passes. Per-segment tallies keyed by ordinal alone
  and block reads of the ordinal columns are the lever; it is the largest cost left on the
  primary path and the next wave.
- **The collapse-path queries are under the 500 ms target without being bounded.**
  `node:self` is 352 to 439 ms at 1M; any query naming `node:`, `state:`, `source:`,
  `repo:`, `rev:` or `hash:`, or sorting on revision, node or state, takes the same path and
  still keeps one row per matching artifact rather than one per page. The primary path is
  bounded and is the model to follow.
- **The usage views are O(rows of the repository).** The client count the USAGE tab shows is
  a claim about every pull the repository ever served, so bounding the chart alone would not
  bound the route: 20,000 rows cost 9.1 ms. It needs a maintained count, not a bound.

Decisions someone has to take:

- **May `hoard doctor` write?** Its `summary` check calls `Catalog::recount_summary`, which
  counts every artifact record again *and upserts the result*, so the check repairs the drift
  it is meant to detect. The repair is already named as such - `src/cli/doctor.rs::summary`
  documents it and the failing check says "totals had drifted and were recounted" - so what
  is open is only that a second run always passes and the drift leaves no trace. Either that
  is accepted, or doctor becomes read-only and something else repairs.
- **Should a rescan ever compact?** `Catalog::compact` runs on a fresh `scan_sources` and
  again at the end of `hoard catalog scan`, and nowhere else; there is no compact subcommand.
  A rescan deliberately does not compact, so a catalog the server holds can be walked again
  without an exclusive borrow - but a long-lived server catalog therefore never reclaims a
  fragmented page. The storage table shows fragmentation running at roughly the stored size.
- **The watcher is not started by `serve`.** `crates/hoard-index/src/watch.rs` exists, is
  tested, and nothing in `src/cli/serve.rs` or `hoard-server` calls it. Phase 10 is partial
  and this is why; decide whether live rescanning ships or the module goes.

Smaller things, each verified:

- The SOURCES tab offers a SCAN button on every row the route lists, including the `mirror`
  and `pulls` rows the ingest and pull paths write. Neither is named by the configuration, so
  `resolve_source` answers 404 and the panel toasts `scan failed`. The button is honest for a
  configured source and dead for those two; whether they should carry one, or appear at all,
  is a UI decision nobody has taken.
- Peer-only artifacts no longer appear in the triage queue. `Catalog::triage_queue` pages
  `state:triage` from the search index, and a peer-advertised artifact is filed as remote
  entries, which never carry that state. Whether a peer's triage should be visible at all is
  an open question, not obviously a bug.
- `docs/feature-plan.md` Phase 1 is named "Core types and index store" and its acceptance
  history was written against the SQLite schema. The bullets now describe the catalog; the
  phase name and the accepted-in-the-past framing are history rather than a description of
  the code.

`docs/native-catalog.md` under "Remaining work" holds the rest: a truncated record panicking
inside `native_model` before any hoard codec runs, `sweep_artifact` decompressing a cold row
only to hand it back to `remove` because native_db 0.8.2 has no remove-by-key, byte-exact
path identities,
`triage_queue` needing rather than tolerating a search index, skipping a rescan's document
re-read when every stamp is unchanged, revision ordinals recomputed on every rescan, counting
a revision's artifact units for content another revision already claimed, complete repository
git metadata with stable cross-scan identities, a read-only redb open once one exists, and
power-loss recovery tests.
