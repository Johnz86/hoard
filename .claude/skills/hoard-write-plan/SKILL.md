---
name: hoard-write-plan
description: Plan a scoped Hoard change against current contracts with observable verification.
allowed-tools: Read, Grep, Glob, Bash, Write
---

Read `AGENTS.md`, `CLAUDE.md` and `CONTRIBUTING.md`, then the owning current references in
`docs/README.md`. Inspect the implementation and existing tests before proposing changes.
Historical performance evidence and prototypes supply context, not current requirements.

## Establish the problem

State the requested user outcome, the observed failure and the smallest scope that resolves
it. Distinguish content identity from selected location and client destination. For UI work,
map the full interaction onto existing Index, inspector, detail and drawer surfaces before
introducing another layout. Define errors, cancellation, pending requests and empty states.

Record the current branch/revision and existing changes. Use the caller's workspace unless
isolation is needed or requested. Do not require a particular branch name, clean tree,
worktree or dispatcher. Preserve unrelated work and follow explicit user constraints.

## Plan format

Use a short plan with:

- Outcome and boundaries: what the user can do when the work is complete.
- Evidence: current behavior, owning code/contracts and the missing regression coverage.
- Interactions and interfaces: visible flow, data ownership and API changes, if needed.
- Implementation steps: focused changes with their dependencies and relevant files.
- Verification: observable success/failure criteria, appropriate Rust checks and affected
  browser/client tasks. UI acceptance includes geometry, density and real acquisition.
- Risks and decisions: compatibility, data preservation and any unresolved material choice.

Use measured performance gates only for a performance task. Create a representative fixture
before measuring; use the benchmark task and a matched baseline rather than old report numbers.
Do not turn every functional change into a scale experiment.

## Keep the plan temporary

Place transient plans in a unique system temporary directory and record ownership. A harness
may supply a scratch location, but none is assumed. Do not commit a plan, completed checklist
or handoff diary. Once implementation settles, update the owning contract and repeatable
verification task, then remove superseded planning notes under
`CONTRIBUTING.md#temporary-work-and-cleanup`.

Do not infer authorization for agents, commits, worktrees, merges, publishing or service
restarts from the existence of a plan. The user request and active environment govern those
choices. A plan should reduce uncertainty and scope, not introduce a separate delivery process.
