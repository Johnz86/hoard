@AGENTS.md

# Hoard

Rust workspace: a local HuggingFace-compatible mirror plus an artifact index for models,
datasets and repos, with a dense keyboard-first web UI. Read these before changing code:

- `docs/feature-plan.md`: scope, phases, acceptance criteria. Work proceeds phase by phase;
  check the phase status table at the top and pick the first unchecked item.
- `docs/architecture.md`: crate boundaries, stack versions, source tree, data flow.
- `docs/native-catalog.md`: the native_db + Tantivy catalog, its layout, its modules and the
  numbers it was last measured at.
- `docs/api/hub-compat.md`: the HuggingFace Hub contract. Do not invent routes or status codes;
  the Python reference at `/home/johnz86/GIT/olah` settles disputes.
- `docs/api/hoard-api.md`: the `/api/v1` JSON contract consumed by the UI and CLI.
- `docs/design/ui-spec.md`: UI tokens, layout, columns, keyboard map extracted from the
  design handoff in `docs/design/handoff/project/`. Match it, do not reinterpret it.
- `docs/handoff.md`: the current state of the repository and what is left to do.

## Crate boundaries

`hoard-core` (types, no IO) <- `hoard-index` (discovery, sniffing, inference, the native
catalog, search) <- `hoard-mirror` (Hub protocol, git face, upstream proxy, block cache) <-
`hoard-net` (node identity, mDNS, digest gossip, peer pulls) <- `hoard-server` (axum wiring,
embedded `web/`) <- `hoard` binary (clap). Lower crates never depend on higher ones and never
on axum. Put new code in the lowest crate that can hold it.

## Compatibility

**There is no release and no user. Nothing needs backward compatibility.**

Every catalog record has exactly one shape, at `native_model` version 1. There are no
migrations, no `From` conversions between record versions, and no encoding snapshots. A
catalog directory whose `FORMAT_VERSION` differs from this build's is deleted and rebuilt
from disk by `Catalog::open_or_scan`, never converted.

When a record changes shape, bump `catalog::FORMAT_VERSION` and change nothing else. Do not
add a version ladder, a converter, or a compatibility shim; if you find yourself writing one,
you have misread this section.

This policy holds until the first release. Changing it is a decision to make deliberately at
that time, and it is made here, in this section. Every other document and skill that touches
the subject points at this section rather than restating it.

## Commands

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace
cargo test --release -p hoard-server --lib assets::
cargo doc --workspace --no-deps
cargo run -- serve --config hoard.toml
```

The first four must pass before a task is reported done. A `PostToolUse` hook runs `rustfmt`
on every edited `.rs` file, so `cargo fmt --check` should already be clean.

`web/` is read from disk in a debug build and embedded in the binary in a release one, so the
debug checks never touch the code a shipped binary runs. The release asset test is the only
thing that does; run it after any change under `crates/hoard-server/` or `web/`.

## Project conventions beyond AGENTS.md

- Dependencies are declared once in the root `[workspace.dependencies]` with a version and
  referenced with `.workspace = true`. Check crates.io for the current version before adding.
- Errors: one `thiserror` enum per crate; `hoard-server` maps them to HTTP in `error.rs`.
- Catalog access only through `hoard_index::catalog::Catalog`, always inside `spawn_blocking`
  when it is reached from an async path. Nothing outside `hoard-index` names a native_db or
  Tantivy type.
- Every writer of a catalog entry moves the derived counts through `scan::DerivedShift` and
  logs its artifacts through `scan::log_changes`. See `.claude/skills/hoard-catalog/SKILL.md`.
- Everything the UI computes (query parsing, facets, Markdown, compat) is a Rust endpoint.
  `web/js` renders JSON and handles input; no logic that could be a unit test lives in JS.
- Fixture files for tests are synthetic headers generated in the test, never real weights.
- No emoji or unicode symbols in source or docs except where the design specifies glyphs.
- Do not touch `docs/design/handoff/`; it is the imported design and stays verbatim.
- Never push, never rebase, never touch another checkout. There is one checkout and one
  branch.

## Agentic development

Work runs as waves: a dispatcher writes a plan, a fresh implementer executes it, the
dispatcher verifies, then a reviewer reviews. The skills carry the contracts so no prompt has
to repeat them.

Skills, in `.claude/skills/`:

- `hoard-write-plan` how to write a wave plan, and where it lives.
- `hoard-implement-plan` the implementer contract: preflight, rules, checks, hygiene,
  deviations, one commit per task, the report.
- `hoard-verify` the check suite, run after every change set.
- `hoard-catalog` what to know before changing the catalog: the module map, the write path,
  native_db and Tantivy semantics.
- `hoard-measure` the measurement protocol: the synthetic tree, the route list, the scale
  harness, the like-for-like rule.
- `hoard-perf-review` the performance reviewer's checklist and report format.
- `hoard-next-phase` start the next unchecked phase of `docs/feature-plan.md`.

Agents, in `.claude/agents/`:

- `wave-implementer` follows `hoard-implement-plan` on the plan path it is given.
- `perf-reviewer` follows `hoard-perf-review` and `hoard-measure`; read-only.
- `rust-reviewer` reviews a change set against AGENTS.md, the crate boundaries and the Hub
  contract; read-only.

The loop: `hoard-write-plan` to `docs/superpowers/plans/YYYY-MM-DD-<name>.md`, dispatch
`wave-implementer` with the plan path and the two commit trailer lines, run `hoard-verify`,
then dispatch `perf-reviewer` or `rust-reviewer`. Delete the plan file when the wave lands;
git keeps it.
