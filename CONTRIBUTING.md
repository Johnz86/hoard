# Contributing to Hoard

Read [AGENTS.md](AGENTS.md) for code quality requirements. Use the
[architecture](docs/reference/architecture.md) to locate the owning crate and the
[API references](docs/README.md#understand-the-system) to identify existing contracts.

## Work from the current contract

Start with the requested outcome and observable acceptance checks. Inspect the implementation
before editing; a dated measurement or old commit is evidence, not a current requirement.
Keep changes focused, preserve unrelated working-tree changes and do not use live catalogs
for development. UI work follows the [UI review workflow](docs/contributing/ui-changes.md).

The crate dependency direction is core, index, mirror, network, server, binary. Shared
computation belongs in Rust; the web UI renders API responses using vanilla JavaScript and
Pico CSS. Reuse existing tables, cards, inspectors and detail layouts.

## Verification

For Rust changes, run from the repository root:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -j 2 -- -D warnings
cargo test --workspace -j 2 -- --test-threads=2
cargo build --workspace -j 2
cargo doc --workspace --no-deps -j 2
```

For changes under `web/` or `crates/hoard-server/`, also verify the embedded release assets:

```sh
cargo test --release -p hoard-server --lib assets:: -j 2 -- --test-threads=2
```

For documentation or task-only changes, run [documentation checks](tasks/check-documentation/README.md)
and the affected task checks. Moving a task requires exercising its entry point after the
move. Avoid repeating unrelated expensive checks when application code has not changed.

The [first-run task](tasks/verify-first-run/README.md) covers configuration, empty states,
owner access, restart and removal. The [recording task](tasks/record-introduction/README.md)
verifies its own find/review/clone journey before rendering media. These checks complement
Rust tests; they do not substitute for broader UI review.

On shared machines, lower command priority and limit build, hashing and test concurrency.
Use temporary directories and available loopback ports. Do not restart existing services,
reuse production catalogs or stop processes you did not start. Benchmarks run serially,
without `RUSTFLAGS` or `target-cpu=native`; see the [benchmark task](tasks/benchmark-catalog/README.md).

## Documentation

- Keep user guides, operations, reference, design and performance evidence in their documented locations.
- Put a repeatable workflow in `tasks/<verb-or-purpose>/`, with a README, entry point, inputs and checks.
- Keep published introduction media in `docs/media/introduction/`; recording inputs belong to its task.
- Update the owning contract and links when behavior or paths change. Avoid duplicate specifications.
- Remove completed task lists, handoffs and superseded proposals; Git retains their history.
- Keep lasting decisions concise: context, decision, consequences and relevant implementation links.
- Date measurements and identify the measured commit, fixture and environment. Never promote old
  benchmark numbers to current guarantees without rerunning the workload.
- Do not embed local machine paths, temporary evidence directories or fixed test totals as current status.
- Preserve prototype HTML/assets as design references; current behavior is specified in the UI reference.

## Changes and commits

Use descriptive commits that state the resulting behavior. Report what changed, the checks
actually run and remaining limitations. Never commit secrets, debug output or temporary
catalogs. Do not push, rebase or modify another checkout as part of a local task unless that
operation was explicitly requested. Optional automation in `.claude/` follows the same contracts.
