@AGENTS.md

# Hoard

Rust workspace: a local HuggingFace-compatible mirror plus an artifact index for models,
datasets and repos, with a dense keyboard-first web UI. Read these before changing code:

- `docs/feature-plan.md`: scope, phases, acceptance criteria. Work proceeds phase by phase;
  check the phase status table at the top and pick the first unchecked item.
- `docs/architecture.md`: crate boundaries, stack versions, source tree, data flow.
- `docs/api/hub-compat.md`: the HuggingFace Hub contract. Do not invent routes or status codes;
  the Python reference at `/home/johnz86/GIT/olah` settles disputes.
- `docs/api/hoard-api.md`: the `/api/v1` JSON contract consumed by the UI and CLI.
- `docs/design/ui-spec.md`: UI tokens, layout, columns, keyboard map extracted from the
  design handoff in `docs/design/handoff/project/`. Match it, do not reinterpret it.

## Crate boundaries

`hoard-core` (types, no IO) <- `hoard-index` (discovery, sniffing, inference, the native
catalog, search) <- `hoard-mirror` (Hub protocol, git face, upstream proxy, block cache) <-
`hoard-server` (axum wiring, embedded `web/`) <- `hoard` binary (clap). Lower crates never
depend on higher ones and never on axum. Put new code in the lowest crate that can hold it.

## Commands

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
cargo test --release -p hoard-server --lib assets::
cargo run -- serve --config hoard.toml
```

All four checks must pass before a task is reported done. A `PostToolUse` hook runs
`rustfmt` on every edited `.rs` file.

`web/` is read from disk in a debug build and embedded in the binary in a release one, so
the debug checks never touch the code a shipped binary runs. The release asset test is the
only thing that does; run it whenever `web/` gains or loses a file.

## Project conventions beyond AGENTS.md

- Dependencies are declared once in the root `[workspace.dependencies]` with a version and
  referenced with `.workspace = true`. Check crates.io for the current version before adding.
- Errors: one `thiserror` enum per crate; `hoard-server` maps them to HTTP in `error.rs`.
- Catalog access only through `hoard_index::catalog::Catalog`, always inside `spawn_blocking`.
- Everything the UI computes (query parsing, facets, Markdown, compat) is a Rust endpoint.
  `web/js` renders JSON and handles input; no logic that could be a unit test lives in JS.
- Fixture files for tests are synthetic headers generated in the test, never real weights.
- No emoji or unicode symbols in source or docs except where the design specifies glyphs.
- Do not touch `docs/design/handoff/`; it is the imported design and stays verbatim.

## Verification loop

Use `/hoard-verify` after every change set. Use `/hoard-next-phase` to start a phase.
