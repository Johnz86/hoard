---
name: hoard-implement-plan
description: The contract for executing a hoard wave plan - preflight, the standing rules, the checks, process hygiene, the deviation record, one commit per task, and the report. Follow it without needing anything else from the dispatcher but the plan path and the commit trailers.
allowed-tools: Read, Edit, Write, Grep, Glob, Bash, Skill
---

You are executing one wave plan in `/home/johnz86/GIT/hoard`. The dispatcher gives you two
things and nothing else is implied: **the plan path** and **the two commit trailer lines**. If
either is missing, ask for it rather than inventing one.

Read the plan in full before the first edit. Read `AGENTS.md` and `CLAUDE.md`. Read
`.claude/skills/hoard-catalog/SKILL.md` before touching `crates/hoard-index/src/catalog/`.

## 1. Preflight

First command, before anything else:

```sh
git status --short && git log --oneline -1
```

Proceed only if the tree is clean apart from untracked plan files under
`docs/superpowers/plans/`, **and** the top commit is the one the plan's gate names. If either
fails, stop and say what you found. Do not "fix" the tree to make the gate pass.

Then: **never push. Never rebase. Never touch another checkout.** There is one checkout and
one branch. The plan file is committed with the first task.

## 2. The standing rules

From `AGENTS.md`, restated because they are the ones that get broken:

- No `//` or `/* */` comments in `.rs` files. Doc comments (`///`, `//!`) only where they
  document a public contract; never to narrate implementation or restate the code.
- No `.unwrap()` outside tests. `.expect()` only for an invariant violation, with a message.
- No emoji, and no unicode anywhere - source, tests, docs, commit messages. ASCII only.
- Exhaustive matches; no catch-all `_` arm where the variants can be named.
- At most five parameters per function; beyond that, a config struct.
- Workspace dependencies are declared once in the root `[workspace.dependencies]` with a
  version and referenced with `.workspace = true`.
- One `thiserror` enum per crate.
- Never `unsafe`; the workspace forbids it. Redesign in safe Rust.
- Fixtures are synthetic headers generated in the test. Never a real weight file.
- Blocking work goes through `spawn_blocking` or `rayon`, never on the reactor.

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

`/hoard-verify` runs the suite and is the shortest way to do this.

**Every commit builds and passes on its own.** If a task's change does not compile without a
file the plan did not list, widen the `git add` to what the build needs - and say so in the
report. A commit that does not build is worse than a wide one.

## 4. Process hygiene

- **Every command in the foreground.** Never start a background process, never leave one
  running. If something needs a long time, raise the Bash timeout.
- A server under test runs under `timeout` **in the same shell command** as the curls that hit
  it. Confirm with `pgrep -af 'target/release/hoard'` afterwards and report the result.
- Benchmarks and the scale harness run one at a time, never with `RUSTFLAGS` and never with
  `target-cpu=native`.
- Temporary files, synthetic trees and scratch catalogs go under the session scratchpad
  directory, never inside the repository. Delete large trees when you are done with them.
- Leave nothing behind: `git status` clean, no `.snap.new`, no scratch file, no stray
  directory under `data/`.

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

One commit per task, with the subject the plan gives, ending with the two trailer lines the
dispatcher supplied:

```sh
git add <exactly the paths this task touched>
git commit -m "<the plan's subject for this task>

Co-Authored-By: ...
Claude-Session: ..."
```

Never `git add -A`. The plan file is committed with the first task.

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
