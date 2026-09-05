# Hoard

Local HuggingFace-compatible mirror and artifact index. Hoard discovers model, dataset and
repository files across local disks, identifies them from their tensor headers, and indexes
them in SQLite. It serves that index through the HuggingFace Hub API, git over HTTP, and a
dense web UI, proxying and block-caching `huggingface.co` for repos it doesn't hold
locally. Nodes find each other on the LAN, gossip their index and pull artifacts peer to
peer; loadouts pin a checkpoint plus its adapters and auxiliaries as a portable text file,
applied to a node as symlinks.

- `docs/use-case.md`: designer's brief
- `docs/feature-plan.md`: scope, phases, acceptance criteria
- `docs/architecture.md`: stack, crate layout, data flow
- `docs/api/hub-compat.md`: HuggingFace Hub routes and error contract
- `docs/api/hoard-api.md`: `/api/v1` JSON API for the UI and CLI
- `docs/design/ui-spec.md`: UI tokens, layout and keyboard map from the design handoff
- `docs/design/handoff/`: the Claude Design export (open `project/Hoard App.dc.html`)

## Agentic development

`CLAUDE.md` imports `AGENTS.md` and points at the docs above. `.claude/` holds the
project settings (permission allowlist, rustfmt hook, enabled plugins), path-scoped rules
for `crates/` and `web/`, the `/hoard-verify` and `/hoard-next-phase` skills, and the
`rust-reviewer` agent. Start a phase with `/hoard-next-phase`.

## Build

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

## Run

```sh
cp hoard.example.toml hoard.toml
cargo run -- serve --config hoard.toml
export HF_ENDPOINT=http://localhost:7099
```
