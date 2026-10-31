@AGENTS.md

# Working on Hoard

Read [CONTRIBUTING.md](CONTRIBUTING.md), then the references relevant to the requested change:

- [Documentation index](docs/README.md)
- [Architecture](docs/reference/architecture.md)
- [Native catalog](docs/reference/native-catalog.md)
- [Hoard JSON API](docs/reference/api/hoard.md)
- [Hub compatibility](docs/reference/api/hugging-face.md)
- [UI specification](docs/design/ui-spec.md)
- [UI change review](docs/contributing/ui-changes.md)
- [Task catalog](tasks/README.md)

## Compatibility

Hoard is pre-release and has no catalog migration guarantee. Each persisted record has one
shape, at native_model version 1. Do not add version ladders, conversion chains or encoding
snapshots. When a stored record shape changes, bump `catalog::FORMAT_VERSION` and update the
single representation. `Catalog::open_or_scan` rebuilds an incompatible catalog from sources;
it does not migrate user-maintained catalog state. Document that operational consequence.
Changing this policy is an explicit project decision, not an incidental implementation choice.

## Implementation contracts

- Respect the dependency direction: core, index, mirror, network, server, binary.
- Use the lowest crate that can own the behavior; lower crates never depend on axum.
- Catalog access outside `hoard-index` uses `Catalog`, inside `spawn_blocking` in async paths.
- Catalog entry writers update derived counters and the search outbox with their records.
  Read `.claude/skills/hoard-catalog/SKILL.md` before editing catalog writers.
- UI computation belongs in Rust endpoints. Vanilla JavaScript renders JSON and handles input.
- Reuse the shared table/cards, inspector, detail tabs/right rail and inline Sources actions.
  Check existing resources and the new journey against before/after evidence.
- Use workspace dependencies and the existing error types. Follow AGENTS.md for formatting,
  tests, comments, naming and error handling. Fixture files are synthetic, never real weights.
- Preserve prototype HTML/support assets in `docs/design/prototypes/`; current behavior comes
  from the UI specification and explicit user decisions.
- Do not push, rebase, restart services, touch another checkout or mutate live data unless the
  requested task authorizes that operation. Limit concurrency on shared machines.

## Verification and documentation

Run the applicable checks in CONTRIBUTING.md and the affected task directory. Web/server
changes require the release asset tests because debug builds read files from disk while
release builds embed them. Do not treat a successful feature journey as broader UX approval.

Update the owning current reference and task instructions with behavior/path changes.
Do not create handoffs, delivery diaries or completed-phase documents. Keep dated performance
results separate from current contracts. Planning notes and transient evidence belong outside
the checkout; Git retains implementation history.

Optional `.claude/` skills and agents support planning, implementation, verification and
performance review. They do not replace the user's scope or imply authorization for extra
agents, worktrees, deployments or external actions.
