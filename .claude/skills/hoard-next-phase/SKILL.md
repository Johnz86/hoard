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
5. Turn that list into a wave plan with `.claude/skills/hoard-write-plan/SKILL.md`, written
   to `docs/superpowers/plans/YYYY-MM-DD-<name>.md`. The phase's acceptance criterion is the
   plan's goal; its bullets are the tasks.
6. Dispatch the `wave-implementer` agent with the plan path and the two commit trailer lines,
   or execute the plan yourself following
   `.claude/skills/hoard-implement-plan/SKILL.md`. Run `/hoard-verify` after each task.
7. When the phase's acceptance check passes, tick its status in the table at the top of
   `docs/feature-plan.md`, add any new module to `docs/architecture.md`, and delete the plan
   file - git keeps it.

Do not start a later phase's work early, and do not widen scope beyond the phase's bullets.
