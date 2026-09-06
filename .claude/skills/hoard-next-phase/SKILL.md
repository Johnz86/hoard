---
name: hoard-next-phase
description: Start the next unchecked phase of docs/feature-plan.md for hoard; reads the plan, architecture and relevant contracts, then turns the phase into a wave plan before any code is written.
allowed-tools: Read, Grep, Glob, Bash, Write, Edit
---

1. Read `docs/feature-plan.md` and find the first phase whose status is not `done`.
2. Read the sections of `docs/architecture.md`, `docs/native-catalog.md`,
   `docs/api/hub-compat.md`, `docs/api/hoard-api.md` and `docs/design/ui-spec.md` that the
   phase touches. Read `docs/handoff.md` for what is already known to be outstanding.
3. List the files the phase adds or changes, using the crate tree in `docs/architecture.md`
   as the source of names. Put each item in the lowest crate that can hold it.
4. For each file, name the unit tests that prove its acceptance criterion.
5. Create the wave's worktree and branch, then turn that list into a wave plan with
   `.claude/skills/hoard-write-plan/SKILL.md`, written untracked into the worktree. The
   phase's acceptance criterion is the plan's goal; its bullets are the tasks.
6. Dispatch the `wave-implementer` agent with the worktree path and the plan path, or execute
   the plan yourself following `.claude/skills/hoard-implement-plan/SKILL.md`. The
   implementer runs the checks itself per task; run `/hoard-verify` in the worktree once when
   the wave returns, before you believe its report. Then land it as one squashed commit on
   `master` the way `CLAUDE.md` "Agentic development" describes.
7. When the phase's acceptance check passes, set its status to `done` in the table at the top
   of `docs/feature-plan.md` - `partial` if some bullets are still open, with the reason in
   the prose under the table - and add any new module to `docs/architecture.md`, inside the
   same wave so it lands in the same commit.

Do not start a later phase's work early, and do not widen scope beyond the phase's bullets.
