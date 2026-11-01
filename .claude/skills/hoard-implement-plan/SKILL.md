---
name: hoard-implement-plan
description: Implement a scoped Hoard plan while preserving current work and verifying the user journey.
allowed-tools: Read, Edit, Write, Grep, Glob, Bash, Skill
---

Read the plan, `AGENTS.md`, `CLAUDE.md` and `CONTRIBUTING.md`. Start in the caller's workspace
or explicitly assigned worktree. Record branch/revision and existing changes; a dirty tree
is not permission to reset, delete, stage or commit unrelated work. No particular branch name,
dispatcher or harness scratchpad is required.

## Implement against current evidence

Check the symbols and behavior a plan names before editing. Follow the current code and
owning contracts when old line numbers, counts or assumptions differ. Make focused changes
that achieve the authorized outcome; routine fixes, test corrections and conflict resolution
remain part of implementation. Raise material scope or data-loss decisions with their concrete
impact rather than silently extending the product.

Read `.claude/skills/hoard-catalog/SKILL.md` for catalog writers and use the relevant
path-scoped rules under `.claude/rules/`. The lessons in
`CONTRIBUTING.md#lessons-to-preserve` apply across search, locations, loadouts and UI state.
Do not duplicate model identity, source layout or path logic in another layer.

## Verify the result

Use the applicable commands in `CONTRIBUTING.md` and the affected `tasks/` entry points.
Web/server changes require embedded release-asset checks. Debug UI success alone does not
verify the shipped binary. Exercise the actual user journey and its important failure cases;
use real isolated client transfers when acquisition changes. Keep fixture hashes and scan
policy consistent when testing identity or persistence.

Fix failures and rerun affected checks. Once checks pass, repeat or broaden them only for a
new change, failure or unresolved concern. Benchmarks require the separate serial protocol
and are not ordinary acceptance tests. Never use production sources/catalogs as fixtures or
stop unrelated services to free a port.

## Finish with current context

Update the owning user, API/design or operations reference, and keep reproducible checks in
`tasks/`. Report what changed, the checks actually run, limitations and remaining working-tree
changes. Do not claim that building activated a running release process.

Commits and branch operations follow the user's scope and any explicitly approved plan;
there is no mandatory checkpoint/squash workflow. Do not push, merge, rebase or touch another
checkout without authorization. If work remains, describe the concrete blocker accurately;
do not create a stale completed-work diary in the repository.

Apply `CONTRIBUTING.md#temporary-work-and-cleanup`: stop only owned processes, verify browser
children exited, and remove reviewed scratch catalogs, captures, logs and plans. Keep intended
implementation changes, production data, durable evidence and the normal release binary.
