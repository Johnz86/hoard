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
- `cargo fmt --check` and `cargo test --workspace` pass: 745 tests, 0 failed, 3 ignored (the
  scale harness and two others). `cargo doc --workspace --no-deps` emits two warnings, listed
  below.

## Running the loop

Work runs as waves. Each wave is one plan file, one fresh implementer agent, a verification
pass, and a review. The skills carry the contracts so no prompt has to repeat them.

1. **Write the plan.** Follow `.claude/skills/hoard-write-plan/SKILL.md`. It goes to
   `docs/superpowers/plans/YYYY-MM-DD-<name>.md`, is committed with the wave's first task,
   and is deleted when the wave lands. Anchor its Verified facts to a named commit and grep
   every symbol it mentions before you hand it over.
2. **Dispatch the implementer.** The `wave-implementer` agent, given the plan path and the
   two commit trailer lines, and nothing else. It follows
   `.claude/skills/hoard-implement-plan/SKILL.md`: preflight, the AGENTS.md rules, the
   checks, foreground-only process hygiene, a record of every deviation from the plan, one
   commit per task, and a report of the commits, the deviations, the check output, the
   measurements and what is left for a human.
3. **Verify.** `/hoard-verify`: `cargo fmt --check`, clippy with `-D warnings`,
   `cargo test --workspace`, `cargo build --workspace`, plus
   `cargo test --release -p hoard-server --lib assets::` after any change under
   `crates/hoard-server/` or `web/`, and `cargo doc --workspace --no-deps` once at the end.
4. **Review.** `perf-reviewer` for a performance wave, following
   `.claude/skills/hoard-perf-review/SKILL.md`; `rust-reviewer` otherwise. Both are
   read-only.

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

- **Allocated disk at 1M is 2276 MB against a 1.5 GB target.** This is the one threshold
  missed by a wide margin, and it is the largest single piece of work left. Revision,
  repository, source and the location key are still written out per entry, which is most of
  what a 1M catalog is made of; interning that text per revision and referencing it by id is
  the lever. The per-table storage breakdown in `docs/native-catalog.md` says where the bytes
  are.
- **The collapse-path queries sit at about 510 ms against a 500 ms target.** `node:self` is
  the measured one; any query naming `node:`, `state:`, `source:`, `repo:`, `rev:` or
  `hash:`, or sorting on revision, node or state, takes the same path. It still keeps one row
  per matching artifact rather than one per page. The primary path is bounded and is the
  model to follow.
- **The usage views are O(rows of the repository).** The client count the USAGE tab shows is
  a claim about every pull the repository ever served, so bounding the chart alone would not
  bound the route: 20,000 rows cost 9.1 ms. It needs a maintained count, not a bound.

Decisions someone has to take:

- **May `hoard doctor` write?** Its `summary` check calls `Catalog::recount_summary`, which
  counts every artifact record again *and upserts the result*, so the check repairs the drift
  it is meant to detect. A second run therefore always passes. Either doctor becomes
  read-only and reports drift without fixing it, or the repair is deliberate and should be
  named as such in the check's output.
- **Should a rescan ever compact?** `Catalog::compact` runs on a fresh `scan_sources` and
  again at the end of `hoard catalog scan`, and nowhere else; there is no compact subcommand.
  A rescan deliberately does not compact, so a catalog the server holds can be walked again
  without an exclusive borrow - but a long-lived server catalog therefore never reclaims a
  fragmented page. The storage table shows fragmentation running at roughly the stored size.
- **The watcher is not started by `serve`.** `crates/hoard-index/src/watch.rs` exists, is
  tested, and nothing in `src/cli/serve.rs` or `hoard-server` calls it. Phase 10 is partial
  and this is why; decide whether live rescanning ships or the module goes.

Smaller things, each verified:

- `/api/v1/sources` filters out every source whose name starts with the peer prefix
  (`catalog/read.rs::sources`), so peer namespaces do not appear in the listing. The SOURCES
  tab in `web/` was not re-checked against that behaviour after the change.
- Peer-only artifacts no longer appear in the triage queue. `Catalog::triage_queue` pages
  `state:triage` from the search index, and a peer-advertised artifact is filed as remote
  entries, which never carry that state. Whether a peer's triage should be visible at all is
  an open question, not obviously a bug.
- Two rustdoc warnings: `public documentation for retract_digest links to private item CHUNK`
  and the same for `reset_stale_pulls`, both in `crates/hoard-index/src/catalog/digest.rs`.
  Either stop linking `CHUNK` or make it public.
- `crates/hoard-net/Cargo.toml` declares `hex.workspace = true` and no code in the crate uses
  the `hex` crate (the only match is a local variable of that name in `digest.rs`).
- `mockall` and `criterion` are declared in the root `[workspace.dependencies]` and no crate
  references either. There are no benches in the workspace.
- `docs/feature-plan.md` Phase 1 is named "Core types and index store" and its acceptance
  history was written against the SQLite schema. The bullets now describe the catalog; the
  phase name and the accepted-in-the-past framing are history rather than a description of
  the code.

`docs/native-catalog.md` under "Remaining work" holds the rest: byte-exact path identities,
`triage_queue` needing rather than tolerating a search index, skipping a rescan's document
re-read when every stamp is unchanged, revision ordinals recomputed on every rescan, counting
a revision's artifact units for content another revision already claimed, complete repository
git metadata with stable cross-scan identities, a read-only redb open once one exists, and
power-loss recovery tests.
