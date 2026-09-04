---
name: hoard-next-phase
description: Start the next unchecked phase of docs/feature-plan.md for hoard; reads the plan, architecture and relevant contracts, then produces a concrete task list before writing code.
allowed-tools: Read, Grep, Glob, Bash
---

1. Read `docs/feature-plan.md` and find the first phase whose status is not `done`.
2. Read the sections of `docs/architecture.md`, `docs/api/hub-compat.md`,
   `docs/api/hoard-api.md` and `docs/design/ui-spec.md` that the phase touches.
3. List the files the phase adds or changes, using the crate tree in `docs/architecture.md`
   as the source of names. Put each item in the lowest crate that can hold it.
4. For each file, name the unit tests that prove its acceptance criterion.
5. Present the task list, then implement it top to bottom, running `/hoard-verify` after
   each crate compiles.
6. When the phase's acceptance check passes, tick its status in the table at the top of
   `docs/feature-plan.md` and add any new module to `docs/architecture.md`.

Do not start a later phase's work early, and do not widen scope beyond the phase's bullets.
