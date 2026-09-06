---
name: hoard-write-plan
description: Write a hoard wave plan a fresh agent can execute without the dispatcher's prompt - the header, Global Constraints, tasks with Files, Interfaces, tests and commits, a Verified facts block anchored to a commit, and a self-review.
allowed-tools: Read, Grep, Glob, Bash, Write, Edit
---

A wave plan is the whole brief for one implementer session. It is written by a dispatcher who
has read the code, and executed by an agent that has not. Everything the implementer would
otherwise have to ask for is in the file.

Write it to `docs/superpowers/plans/YYYY-MM-DD-<name>.md`. It is committed with the first
task and **deleted when the wave lands** - git keeps the history, and a stale plan file in the
tree is a trap for the next agent. Say in the handoff that you deleted it.

## The skeleton

```markdown
# <Title>

**Goal:** the end state in one or two sentences, in the indicative.

**Architecture:** one paragraph naming the mechanism - which module gets what, which record
changes shape, which path stops paying which cost.

**Tech Stack:** Rust 2024 plus the pinned crate versions this plan touches.

**Spec:** backticked paths to the docs that settle disputes, with the section in parentheses.

## Global Constraints

- ...

---

### Task 1: <a title that states the outcome>

**Files:** ...

**Interfaces:** ...

  <an indented paragraph spelling out the semantics in full>

- [ ] Test `<name>`: ...
- [ ] Commit: "<subject>".

---

### Task 2: ...

---

## Self-review

- ...
```

Only `## Global Constraints` and `## Self-review` are `##`. Tasks are `###`, numbered from 1,
or from `### Task 0` when the wave opens on a baseline measurement. Two to five tasks is the
usual size. `---` separates the constraints from the first task and every task from the next.

## Global Constraints

Six bullets, in this order. The implementer skill
(`.claude/skills/hoard-implement-plan/SKILL.md`) carries the standing contract, so these
bullets carry only what changes per wave.

1. **The gate.** Where to work, which branch, the preflight command, and the stop condition:

   > Work in `/home/johnz86/GIT/hoard` on `master`. First command:
   > `git status --short && git log --oneline -1`; proceed only if the tree is clean apart
   > from this plan file and the top commit is `<hash>` "<its subject>". Commit this plan
   > file with Task 1. Never push.

   Give the commit as hash **and** subject. Gating on the subject alone is acceptable when the
   hash is not yet known at writing time; gating on nothing is not.

2. **The checks.** Name any that are extra for this wave, and where in the wave they run:
   the release asset test after a server change, `cargo doc --workspace --no-deps` after the
   last task, a `cargo tree -i <crate>` that must report the package is gone.

3. **The conventions.** One line pointing at AGENTS.md, plus only the clauses this wave can
   trip over: exhaustive matches with no catch-all on a named enum, handlers returning
   `ApiResult`, blocking catalog calls through `spawn_blocking`, `clippy::print_stdout`
   denied so the CLI writes through a locked stdout handle.

4. **The contract fence.** What must not change: "No route or command changes its JSON shape,
   ordering or contract. Every existing test keeps its assertions unless it asserted the cost
   being removed; say which."

5. **The commit trailers.** The two lines, verbatim, in a fenced block: the `Co-Authored-By:`
   and `Claude-Session:` lines this session was given for attribution. The dispatcher copies
   them from its own instructions; the implementer never invents them.

6. **Verified facts.** See below.

Add, when the wave calls for it: a scope fence naming exactly what stays behind and what may
not be called; numeric thresholds to beat, quoted with their previous numbers; a scratchpad
placement rule for synthetic trees; a pinned expectation list of existing test functions that
must keep their assertions.

## Verified facts

The last constraint bullet, and often the longest thing in the file. It freezes what the
implementer would otherwise re-derive: exact symbol names, field lists, line ranges, file and
occurrence counts, and for every ambiguous pair an inline verdict.

- Anchor it to the commit the gate names: `- Facts at \`<hash>\`:`.
- Give counts, so the implementer can prove they found everything: "41 `impl From` blocks",
  "15 tests named `*_encoding_is_frozen`", "23 files under `snapshots/`".
- Annotate every ambiguity inline: "`recount_summary` is also used by `hoard doctor` and the
  fresh scan (keep it); `recount_derived` is migration-only (delete it)".
- Add a staleness disclaimer when the facts may have moved: "re-check each with `grep` before
  editing".
- A second bullet for **library facts** - invariants of a dependency that shape the design
  rather than repo state. For the catalog, do not restate them: point at
  `.claude/skills/hoard-catalog/SKILL.md` and name only the ones this wave turns on.

Facts go stale. The implementer skill requires the implementer to grep every symbol the plan
names before each task and to record every deviation; write the facts so that check is cheap.

## Tasks

**Files:** first, always. Either one prose line of backticked paths with brace expansion and
a parenthetical note per file, or a bullet list tagged `Modify:` / `Create:` / `Delete:` /
`Test:` with the same notes. Say when a file is on the list only to be read:
"`crates/hoard-index/tests/scale.rs` (read the thresholds; do not change them)".

**Interfaces:** when a type or signature changes. A fenced `rust` block with the exact
declaration, or `- Produces:` / `- Consumes:` bullets that also record which task a type comes
from, so the tasks can be read in order.

**The indented paragraph** after the interfaces is where a plan earns its keep: which writer
maintains which field, which function is deleted and why it is safe, what an error message
says, what happens on the path nobody thinks about.

**Steps** are `- [ ]` checkboxes. Two flat steps per task is enough for a well-specified one:
the test, then the commit. Name every test by its final snake_case function name, reading as a
sentence - `a_catalog_of_another_format_is_rebuilt_on_open`,
`repository_counts_follow_every_writer` - and state its assertions, not its intent. Give a
test body as a compilable `rust` block when the setup is not obvious. When the task is
delicate, use the longer TDD form: write the failing test, run it and state the expected
failure verbatim, implement, run, commit.

**Every task ends with a commit**, one commit per task, with the subject given in the plan:
imperative, sentence case, no prefix, no trailing period, saying what the code now does rather
than what the author did. "Answer the triage queue from the search index", not "Fix triage
performance".

**Specify deletions as precisely as additions**, and say how to prove one is complete: a
`grep` with its expected empty output, or a `cargo tree` that must not find the package.

**Pre-authorise the escape hatches**: "skip and say so if `hoard.toml` is absent", "verify
with grep and skip what is already done", "simplest acceptable outcome: ...". An implementer
that hits an unauthorised fork in the road either guesses or stalls.

The first task is often bookkeeping - commit the plan file, take the before measurement. The
last is verification and recording, never new behaviour.

## Measurement tasks

If the wave is about performance, Task 0 is the before measurement and the last task is the
after measurement plus the doc. Do not restate the protocol in the plan: point at
`.claude/skills/hoard-measure/SKILL.md` and name only what is specific - the routes added to
the fixed list, the fixture to seed, the previous numbers to beat, the section heading in
`docs/native-catalog.md` to write under.

Require the verdict to state each threshold as met or missed **with the number**, and add the
anti-softening rule: *do not soften a miss; name the largest remaining cost you can identify
from the code rather than guessing.*

## Self-review

Three or four bullets, no checkboxes, at the end. Not a checklist of steps: **properties that
must hold when the plan is done, written so a reviewer can falsify them.** Two shapes work:

- Coverage, consistency, out of scope: which task covers which part of the spec; which names
  and types must match between tasks; what is deliberately left for a later wave.
- Grep-checkable end states: "After Task 3
  `grep -rn 'migrate\|V[0-9]' crates/hoard-index/src/catalog` returns nothing"; "Task 4 leaves
  the repository clean and every scratchpad empty".

## Before you hand the plan over

- Every symbol the plan names exists at the commit the gate names. Grep for each one.
- Every task is committable on its own: it builds and its tests pass without the next task.
- Nothing in the plan restates the implementer contract, the check list, the catalog facts or
  the measurement protocol; each of those is a pointer to its skill.
- The file is ASCII: `grep -nP '[^\x00-\x7F]' <the plan>` returns nothing.
