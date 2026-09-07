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
- Phases 0 to 11 are done. `docs/feature-plan.md` holds the table. The TREE and DISK views
  are not an unfinished phase: they wait on a design decision and are recorded under that
  document's "Risks and open decisions".
- At the head of `master` after the disk wave all four checks pass:
  `cargo fmt --check`, clippy with `-D warnings`, `cargo test --workspace` at 862 passed,
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
- **The disk wave took the three record levers and closed the fourth.** A revision's card is
  one `RevisionText` row keyed by `(repo_id, revision)` rather than a copy in every
  artifact's `SearchText`; every record but `Manifest` is on the compact codec, `Manifest`
  alone keeping native_model's default so a format mismatch is found before any compact row
  is decoded; and `ArtifactDetail` keeps the sixteen tensor samples the triage queue reads a
  hidden dimension from rather than the sixty-four the sniff draws for inference.
  That wave bumped `FORMAT_VERSION`, so every existing catalog is rebuilt from disk. Nothing
  was measured: the four rows for wave 8 to watch are allocated disk at 1M, the rebuild (one
  card read per revision, not per artifact), the inspector detail row, and the supplemental
  fixture with cards, which loses most of its `SearchText`.
- **Page packing is answered and not worth doing.**
  `docs/superpowers/research/2026-09-page-packing.md` reads redb 2.6.3: the 42 to 55 per cent
  leaf fill comes from one split rule that always cuts at the byte midpoint and has no append
  case, so insertion order, a bulk load in key order and a dense rewrite all land at the same
  fill, `Database::compact` moves whole pages and never rebuilds a leaf, and
  `Builder::set_page_size` is compiled only under `cfg(any(fuzzing, test))`. The reachable
  saving is 0 to 5 per cent; the lever that works is payload, at about two bytes of file per
  byte off a row.
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
- **Every query path is bounded by the page and the sample as of wave 4, and the numbers are
  not in yet.** The collapse path folds each artifact's run of entry documents inside the
  segment collectors instead of retaining a row per matching artifact, a primary query no
  longer counts its match in a pass of its own, and the base rail is capped. The final
  measurement of the run is what says what that bought. The harness carries the rows for it:
  `query node:self midpoint offset`, `query node:self deep offset` and `resident rss over the
  collapse queries` were added in wave 4 and have never been run, so wave 8 reports them
  against nothing. The first, last and middle primary pages are the rows that should move for
  the pass count.
- **Two things wave 4 traded, both recorded in `docs/native-catalog.md` under "Remaining
  work".** A shallow window is no longer cut from the far end, because the pass that counts
  it is the pass that keeps it, so a page just inside the shallow bound and past the middle
  of a small result holds `offset + limit` rows rather than `total - offset`; it is bounded
  by the shallow constant and the fix is named there. And the index writer now sends one
  operation group per artifact rather than per document, which is what makes an artifact's
  documents one run in one segment, so tantivy's 10,000-item pipeline now buffers 10,000
  artifacts' documents rather than 10,000 documents; the synthetic tree cannot show it, and
  wave 8 should read the rebuild RSS row knowing it.
- **A revision the walk no longer finds keeps its slot forever.** An entry names its
  revision by ordinal, so the slot is an identity and cannot be reclaimed while any entry
  names it. A repository whose branch moves on every scan grows one stored revision per old
  head, each carrying its title, author and commit but no refs. Reclaiming a slot needs the
  entries of a retired revision removed rather than marked absent, which is the same decision
  as pruning a deleted path's row; neither is taken.
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
  before either can be implemented and are a risk rather than a phase. Phase 11's storage replacement is complete; the optimisation
  that continues is a list of follow-ups with their own acceptance criteria under `### Phase
  11`. **Done in this wave.**
- **How should the base facet be counted?** Taken by default in wave 4: the rail is
  truncated to the 32 most populous bases by tally before the per-bucket count runs, so the
  searches a query pays for the rail are bounded by a constant rather than by how many bases
  the catalog holds. The prefix rule is unchanged, so a bucket's count is still what a click
  on it answers; a base outside the 32 is not shown and is not marked active, and a query
  naming it still answers. `docs/api/hoard-api.md` says so. Answering the prefix rule from
  ordinals, which would change what a base bucket means, remains the other way out and is
  still not an implementer's call. **Taken in wave 4.**

Smaller things, each verified:

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
  function with the path's own name. `FORMAT_VERSION` is 6, because a blob's inode, mtime and
  size do not change when the rule that reads it does and a rescan would settle every stored
  revision on its stamps; the bump rebuilds every existing catalog from disk, which is what
  puts the cards into one. Wave 8's workstation scan therefore reads and indexes the READMEs
  and configs of every cached revision: its scan time, its card count and its search answers
  all move, and nothing in the synthetic tree does, because the tree has no hf-cache source.
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
