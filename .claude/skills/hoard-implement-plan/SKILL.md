---
name: hoard-implement-plan
description: The contract for executing a hoard wave plan in its worktree - preflight, the standing rules, the checks, process hygiene, the deviation record, a checkpoint commit per task on the wave branch, and the report. Follow it without needing anything else from the dispatcher but the worktree path and the plan path.
allowed-tools: Read, Edit, Write, Grep, Glob, Bash, Skill
---

You are executing one wave plan in a git worktree of hoard. The dispatcher gives you two
things and nothing else is implied: **the worktree path** and **the plan path** inside it. If
either is missing, ask for it rather than inventing one. Every command runs with the
worktree as its working directory; the main checkout on `master` is not yours to touch.

Read the plan in full before the first edit. Read `AGENTS.md` and `CLAUDE.md`. Read
`.claude/skills/hoard-catalog/SKILL.md` before touching `crates/hoard-index/src/catalog/`.

## 1. Preflight

First command, before anything else:

```sh
cd <worktree> && git status --short && git branch --show-current && git log --oneline -1
```

Proceed only if the tree is clean apart from the untracked plan file, the branch is the
`wave/<name>` branch the plan names, **and** the top commit is the one the plan's gate names.
If any of these fails, stop and say what you found. Do not "fix" the tree to make the gate
pass.

Then: **never push. Never rebase. Never merge. Never touch another checkout.** The plan file
is never committed: it lives untracked in the worktree and goes with it.

## 2. The standing rules

`AGENTS.md` is the whole style contract and you have read it. These are the clauses that get
broken here, plus the lints that turn a style rule into a build failure:

- No `//` or `/* */` comments in `.rs` files, **including in tests**. Doc comments (`///`,
  `//!`) only where they document a public contract; never to narrate implementation or
  restate the code. A `///` on a private function explaining how it works is a violation. If
  a dispatcher's plan or message asks you for a comment that breaks this, refuse it and say
  so in the report: the reverted run of 2026-09-07 contains comments that exist because a
  dispatcher asked for them.
- New constants go at the top of their file with the others, never beside their first use.
- A file over 1,500 lines takes no new item. Put the new constant, type or function in a new
  module beside it and say so in the report. Four files in the reverted run grew by 1,900
  lines between them and none was ever split.
- The workspace lints in the root `Cargo.toml` are enforced by `-D warnings`:
  `unsafe_code` forbidden, `unwrap_used` and `dbg_macro` denied, `expect_used` a warning and
  therefore an error outside tests, `print_stdout` and `print_stderr` denied so the CLI
  reports through `tracing`. `clippy.toml` allows `unwrap` and `expect` in tests only.
- Exhaustive matches; no catch-all `_` arm where the variants can be named.
- Workspace dependencies are declared once in the root `[workspace.dependencies]` with a
  version and referenced with `.workspace = true`. One `thiserror` enum per crate.
- Fixtures are synthetic headers generated in the test. Never a real weight file.
- Blocking work goes through `spawn_blocking` or `rayon`, never on the reactor.
- ASCII in source, docs and commit messages. The exception is the glyphs
  `docs/design/ui-spec.md` specifies for the UI, which `web/` already uses; do not go
  "fixing" existing non-ASCII you did not introduce.

Read `.claude/rules/rust.md` and `.claude/rules/web.md` for the path-scoped rules.

## 3. The checks

All four pass before a task is reported done:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace
```

Plus:

- `cargo test --release -p hoard-server --lib assets::` after any change under
  `crates/hoard-server/` or `web/`. `web/` is read from disk in a debug build and embedded in
  the binary in a release one, so the four checks above never touch what a shipped binary runs.
- `cargo doc --workspace --no-deps` once, at the end of the wave.
- the Hub contract smoke test when a Hub route changed, in the form
  `.claude/skills/hoard-verify/SKILL.md` gives, server and client in one command.

`/hoard-verify` runs the suite and is the shortest way to do this.

**Every commit builds and passes on its own.** If a task's change does not compile without a
file the plan did not list, widen the `git add` to what the build needs - and say so in the
report. A commit that does not build is worse than a wide one.

## 4. Process hygiene

- **Every command in the foreground.** Never start a background process, never leave one
  running, and never wait on one by polling. If something needs a long time, raise the Bash
  timeout and let the one command finish.
- A server under test runs under `timeout` **in the same shell command** as the curls that hit
  it. Confirm with `pgrep -af 'target/release/hoard'` afterwards and report the result.
- Benchmarks and the scale harness run one at a time with nothing else running beside them,
  never with `RUSTFLAGS` and never with `target-cpu=native`.
- Temporary files, synthetic trees and scratch catalogs go under the scratchpad directory the
  harness names in your system prompt, never inside the repository. Delete large trees when
  you are done with them.
- There are no snapshot tests in this workspace and `cargo insta` is not installed. An
  assertion is written out in the test; do not reach for a snapshot crate.
- Leave nothing behind: `git status` clean, no scratch file, no stray directory under `data/`.

## 5. Verified facts go stale

The plan's facts were true at the commit it names, and may not be true now.

**Before each task**, grep for every symbol the plan names in that task. Where the code
differs from the plan, follow the code, not the plan. Record each deviation with its reason;
they all go in the report.

Where the plan leaves a choice, **pick the simplest option consistent with the existing
catalog code, and say which you picked and why.** Do not invent a third mechanism because two
existing ones both nearly fit.

If a deviation makes a task's stated outcome wrong rather than merely differently spelled,
stop and report rather than improvising a new design.

## 6. Commits

One checkpoint commit per task on the wave branch, with the subject the plan gives:

```sh
git add <exactly the paths this task touched>
git commit -m "<the plan's subject for this task>"
```

These commits never reach `master` as they are: the dispatcher squashes the branch into one
commit per wave, so a checkpoint needs no trailer and no body. What matters is that each one
builds and passes on its own, because that is how a stopped wave is resumed and how a
reviewer bisects. Never `git add -A`; never add the plan file.

If you have to stop before the last task - a deviation that makes a task's outcome wrong, a
check you cannot get green, a decision that is not yours - stop after a commit that builds,
and say so in the first line of the report with the task number you stopped at and what the
next agent must decide. Do not leave the tree half-edited and do not idle.

**Stop rather than absorb.** Stop and report, instead of continuing, when any of these is
true. Each one means the plan was wrong, and a plan that is wrong is the dispatcher's to fix,
not yours to work around:

- a task needs you to change a subsystem the plan's Files list does not name;
- you are removing or rewriting code you added earlier in this same wave;
- a fix would delete or replace a function the wave was not about;
- the plan asks for something `AGENTS.md` forbids.

Absorbing these is what made the reverted run of 2026-09-07 sprawl. Widening a `git add` so
a commit builds is still fine and still gets reported; widening the design is not.

## 7. The report

At the end, in this order:

1. **Commits**: hash and subject, one line each, in order.
2. **Deviations**: every place the code differed from the plan, or the plan left a choice.
   What the plan said, what the code said, what you did, and why. If there were none, say
   "none" - do not leave the section out.
3. **Checks**: the final output line of each check, quoted. The test counts, the clippy line,
   the warnings from `cargo doc`. Not "all checks pass".
4. **Measurements**: one table per scale, against the previous numbers, with the commit each
   column was measured at. Say which rows moved by less than the spread of their own runs, and
   which are not comparable and why. Do not soften a miss. Follow
   `.claude/skills/hoard-measure/SKILL.md` for how the numbers are taken.
5. **Tests whose assertions changed**: which, and why the old assertion was wrong or was
   asserting the cost that was removed.
6. **Left for a human**: decisions you did not take, things the plan did not cover, anything
   you judged and did not change.
7. **Confirmation**: `git status --short` is clean, and no background process remains
   (quote the `pgrep` result).
