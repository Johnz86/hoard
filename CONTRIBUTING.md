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

## Lessons to preserve

- A model's content identity, selected file location and client destination are different
  facts. Search all filename/path aliases while counting content once; carry the matching
  entry through inspection, deep links, commands and collection additions. Validate entry
  ownership and refuse stale explicit selections rather than silently choosing another copy.
- Display and sort the same label. Test identical basenames in different folders, renamed
  copies in different sources and mixed-resource pagination. A successful scan does not
  prove that the user can recognize or find its files in the Index.
- Known tool categories retain their repositories when their last top-level file moves into
  a subfolder. Keep layout classification separate from model role and publisher identity.
- Loadouts group files for client acquisition on a single-server setup. Keep new defaults
  relative to the known category, preserve nested folders and never rewrite saved destinations.
  The loadout UI does not invoke the legacy server projection APIs.
- Condensed UI needs measured capacity, aligned controls and details available by click or
  keyboard. Preserve drafts, focus and scroll during updates; guard asynchronous completions
  after a view closes or is replaced. Test delayed requests and actual file acquisition.
- A derived search-schema change rebuilds search from existing catalog records. A persisted
  record change follows the catalog format policy and can discard catalog-only state.
  Do not trigger a full catalog rebuild to repair search-only behavior.
- A release build embeds the UI; building it does not update an already running process.
  Respect the operator's service lifecycle. Sharing-address edits are a separate runtime
  setting and do not require restarting Hoard or changing host networking.

Owning details live in the [UI specification](docs/design/ui-spec.md),
[source layouts](docs/reference/source-layouts.md), [catalog reference](docs/reference/native-catalog.md)
and [loadout contract](docs/design/loadouts.md). Update those contracts with behavior changes.

## Temporary work and cleanup

Record the initial working-tree changes and the paths/PIDs created for the task. Use a unique
system temporary directory for plans, synthetic sources, catalogs, client caches, browser
profiles and captures; no harness-provided scratchpad path is assumed. Keep fixtures out of
production sources and use temporary configuration with available loopback ports.

Review evidence before removing it. Shut down test browsers through their control connection
and stop only owned processes; killing a launcher wrapper may leave browser children running.
Check that those processes have exited before deleting their directories. Never kill all
Hoard/browser processes or clear all of `/tmp` to tidy one task.

After review, retain lasting behavior and lessons in the owning docs, repeatable checks in
`tasks/`, and deliberate published media/performance evidence in their documented locations.
Remove scratch plans, handoffs, failed-run catalogs, captures, logs and unused alternate build
directories once they are no longer needed. Preserve source data, configuration, dependencies,
the normal build cache and the release binary. Do not discard uncommitted implementation
changes or manufacture a clean tree by resetting, deleting or committing them.

Describe remaining changes and verification accurately in the final response. Git records
the delivery history; current docs should not become a session diary or a list of completed work.

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
