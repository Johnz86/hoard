# Goal: predictable memory under unusual workloads, in waves

Set on 2026-09-07 at `9979f7b`, after the remaining-work run finished and its review
(`docs/superpowers/history/2026-09-remaining-work-review.md`) was written. This document is
the brief for every wave that follows, the finish condition that ends the run, the rule that
performance is measured once at the end, and the decisions the dispatcher takes so no wave
waits on one. `docs/handoff.md` points here.

## What this run is about

The catalog meets every standing gate at 1M on the synthetic tree. What the tree cannot show
is what this run is for: memory that stays predictable when the workload is unusual, background
work that yields, and writes that are not repeated for nothing. The owner's ranked review of
2026-09-07 named eleven items. At `9979f7b` these are already done and are not in this run:
bounded mass retirement (`retract_absent` commits in `CHUNK`-sized transactions), a hashing
pass that stops within a megabyte when the lane closes, the deep window counted before it is
sampled, the two routes that were not slower, the usage chart read off the repository record,
and the rescan's resident maps bounded per repository.

What is left, and is code:

- **Hashing dispatch.** `read_digests` still hashes 64 files as one `par_iter` and waits for
  the whole batch before the next is read, so one enormous file idles every other worker.
  The concurrency is the global rayon pool with no setting.
- **The index writer's pipeline is bounded in artifacts, not bytes.** Tantivy's operation
  channel holds 10,000 groups and a group is one artifact's documents, each carrying the
  revision card, so the documents in flight scale with entries per artifact times card size.
- **The fresh scan's memory scales with the source.** Discovery materialises every entry of
  a source before the walk starts, and the counters keep one summary snapshot per artifact
  touched until the pass ends. At 1M that is most of the 860 MiB the scan grows by; the
  page cache is another 256 MiB.
- **Recent pulls materialise a repository's whole key range.** `newest_usage` reads the
  `repo_at` secondary key, which native_db collects in full before yielding the last page.
- **Usage updates re-index served artifacts on every refresh.** Each served artifact logs a
  search change and the next refresh deletes and re-adds every document of it, card included,
  once per two-second batch for as long as it is served.
- **The usage sink has a flush trigger and no bound.** `Debounced::record` grows its vector
  for as long as a write is delayed.
- **The page cache size is a constant** and hashing concurrency is not a setting.
- **Nothing measures concurrent searches, a mass deletion, or a rebuild with cards.** The
  harness runs one query at a time, rewrites 1,000 headers and never deletes, and the gate
  tree has one entry per artifact and no card.

Deliberately not code in this run: retention of retired metadata (tombstoned revisions,
removed paths, old usage) stays as it is until the first release decides it, as the review
recommended; native_db improvements (lazy secondary scans, remove-by-key, secondary keys
updated only when changed, watcher events skipped when disabled) are upstream work outside
this loop; dense page packing stays a design; the TREE and DISK views have no design.

## How the loop runs in this run

The loop is the one `CLAUDE.md` "Agentic development" describes, with these rules on top:

- **The implementer is Claude Sonnet.** Every wave is executed by the `wave-implementer`
  agent dispatched with the model override `sonnet`. The dispatcher writes the plan,
  verifies with `/hoard-verify`, dispatches the reviewer, lands the squash and takes every
  decision. The reviewers keep their own models.
- **The dispatcher does not implement.** A wave that stops early is resumed with a message
  naming the next task; a fix from a review is a task appended to the same wave for the same
  implementer, never an edit by the dispatcher. The dispatcher edits only plans, this goal,
  and what the landing commit must carry.
- **The three loop rules the review asked for apply from wave 1.** Before handing a plan
  over, the dispatcher runs the plan's gate greps itself and, for every invariant the plan
  claims, names the code that enforces it in the Verified facts. Every plan lists, under a
  pinned-tests bullet, the tests of the waves it builds on that must keep their assertions,
  and the reviewer brief names those waves. Any draft that must survive a session goes in
  the worktree beside the plan, never in the session scratchpad.
- **Measure once, at the end.** No wave before the last runs the scale harness or the route
  list. A wave that changes a measured path states in its report which rows it expects to
  move and why. A wave that adds a workload to the harness runs it once at 100k to prove the
  workload runs, reports the number as a smoke figure, and does not record it.

## The waves

Each wave is one plan, one worktree, one Sonnet implementer, verification, one review, one
squashed commit on `master`, in this order.

1. **hash-dispatch.** Continuous bounded dispatch in the hashing pass: a producer pages the
   files owing a hash in primary key order and feeds a bounded queue; a fixed set of workers
   pulls one file at a time and sends each digest to the writer that already commits every
   64; no file waits for another. `[index] hash_threads` sets the worker count, default the
   rayon pool's size. The stop is read where it is read now, per batch of the writer and per
   megabyte of every read. Proven by a fixture whose one blocking file (a FIFO written by the
   test after every other file has been committed) cannot hold the rest back, by the
   existing stop and kill tests kept, and by `hash_threads = 1` hashing in key order.
   Plus the loop rules written into `.claude/skills/hoard-write-plan/SKILL.md` and the
   reviewer agents, and the handoff pointing here.
2. **index-bytes.** The rebuild and the refresh meter the bytes of the documents they hand
   to Tantivy and commit whenever the estimate since the last commit passes a budget, so the
   documents in flight are bounded in bytes whatever an artifact's entry count or card size.
   An artifact stays one operation group, so the fold's run invariant holds. The refresh
   reports what it did: changes applied, documents written, bytes estimated, commits made,
   and the coordinator and the refresher log it. Proven by a seeded fixture of large cards
   and many entries per artifact that lands in more than one segment under a test budget
   and answers the collapse queries the same, and by a supplemental harness row for the
   rebuild's peak RSS with cards that the last wave records.
3. **usage-bounds.** The usage log keyed by repository and sequence, so the newest pulls of a
   repository are one lazy reverse primary range and the `repo_at` secondary index goes;
   `Usage.seq` becomes per repository, allocated on the repository's own record, with the
   rows nobody attributes under repository id 0. The `last_used` an artifact carries into
   the index is floored to a minute, and a usage batch logs a search change for an artifact
   only when that minute moves, so an artifact served continuously is re-indexed once a
   minute rather than once a batch. The usage sink gets a hard bound: past it, a recorded
   response is folded into the counts the next write applies (pulls per repository, per
   day and per client, uses per artifact) with no pull row, and one warning per overflow
   episode says how many were folded. `FORMAT_VERSION` bumps.
4. **scan-scope.** The scan works in groups of repositories: discovery yields a source's
   repositories in groups, each group is stored, walked and, for a rescan, its absent paths
   retracted before the next group is discovered, and the known-stamp map is loaded per
   group; repositories the catalog holds for the source that discovery never yielded are
   retracted after the last group. The summary delta is folded per transaction rather than
   kept per touched artifact until the pass ends. `[index] cache_mib` sets the redb page
   cache, default 256. Every recovery, refile and revision-identity test keeps its
   assertions. The 1M scan RSS row is what the last wave reads it on.
5. **budgets-measured.** The harness gains what this run must measure: a deletion phase
   that removes a tenth of the tree and rescans, reporting wall and peak RSS growth; a
   concurrent-search phase that runs eight midpoint queries at once and reports peak RSS
   growth over them; a mixed-size hashing row with one multi-gigabyte sparse file beside a
   thousand small ones. Then the whole protocol of `.claude/skills/hoard-measure/SKILL.md`
   against `9979f7b` rebuilt in the same session, the supplemental fixture with cards and
   many entries per artifact, and a fresh scan of this workstation's disks with `hoard
   doctor` after it. Every threshold stated as met or missed with the number, in
   `docs/native-catalog.md`. A miss produces a fix wave and a re-measurement of the missed
   rows only.
6. **fix-wave**, if the measurement asks for one: the missed rows, and what the closing
   reviews find across the whole delta, fixed or recorded.

After the last wave: `rust-reviewer` and `perf-reviewer` review the whole delta from
`9979f7b` to the head, their findings are fixed or recorded, and the dispatcher writes the
review under `docs/superpowers/history/`.

## Decisions taken

Taken on 2026-09-07 by the dispatcher, under the owner's instruction that the dispatcher
pushes its recommended decisions rather than waiting. The owner can overturn any of them
before its wave opens; after it lands, overturning is a wave of its own.

1. **Hashing concurrency** is `[index] hash_threads`, a worker count; `0` or absent means
   the rayon pool's size, which is what runs today. No per-device scoping in this run: the
   setting is what an HDD or NAS owner turns down, and a measurement of one such disk is what
   would justify more.
2. **The index writer's budget** is 128 MiB of estimated document bytes between commits, on
   both the rebuild and the refresh. The estimate is the sum of every text value a document
   carries plus a fixed allowance per document; it does not need to be exact, it needs to be
   monotone in the card and the entry count.
3. **The usage sink's bound** is sixteen times its flush capacity, 4,096 responses. Past it,
   a response is folded into counts and loses its pull row. Nothing is silently discarded:
   the fold is counted and warned about once per episode, and the counts the USAGE tab and
   the `used:` token read stay exact.
4. **The index's `last_used` grain** is one minute. The catalog keeps the exact time; only
   the change log and the indexed value are floored.
5. **The scan's unit of memory** is a group of 64 repositories, and a single repository of
   any size stays one unit: a loose repository of a million files is discovered whole, as
   today, and the run records that rather than splitting a repository.
6. **The page cache** is `[index] cache_mib`, default 256, the value the catalog has used
   since the native catalog landed.
7. **Retention** is not taken in this run. Tombstoned revisions, removed paths marked
   `Remote` and historical usage stay until the first release decides a lifetime for them.
8. **native_db upstream work** is not in this run. `sweep_artifact` keeps decoding a cold row
   to delete it and every artifact update keeps upserting its text and detail rows.
9. **The before column** of the final measurement is `9979f7b` rebuilt in the session. A row
   this run adds to the harness has no before column and the table says so.

## Finish condition

The run is finished when every line holds:

1. Waves 1 to 5 are on `master`, one commit each, plus wave 6 if the measurement asked for
   it; each implemented by a Sonnet `wave-implementer`, verified by
   `.claude/skills/hoard-verify/SKILL.md` and reviewed before landing.
2. Every test of the remaining-work run keeps its assertions except where a plan says which
   assertion asserted a cost this run removed.
3. `docs/handoff.md` points here, its "Follow-ups" hold only what this document names as
   deliberately not code, and `docs/native-catalog.md` describes the code at the head:
   the hashing dispatch, the byte-metered writer, the repository-keyed usage log, the
   grouped scan and the new settings.
4. The last wave's tables show every standing gate met at 1M against `9979f7b` measured
   beside them: allocated disk under 1,500,000,000 bytes at rest and at the rebuild peak;
   every query, the collapse queries and the midpoint pages included, under 500 ms; peak
   scan RSS growth under 2 GB and lower than the `9979f7b` column by more than its spread;
   rebuild no slower than the scan; no route of the eleven slower than the column beside it
   by more than its spread without a stated reason; and the new rows recorded with the
   number: the deletion phase's wall and RSS, the concurrent-search RSS, the mixed-size
   hashing wall, the rebuild RSS with cards.
5. `hoard doctor` passes every check on this workstation's catalog after the fresh scan of
   wave 5, and `--repair` reports no drift.
6. The two reviewers report no finding ranked above minor that is not fixed or recorded with
   a reason.
7. The review is written under `docs/superpowers/history/` and the handoff points at it.
