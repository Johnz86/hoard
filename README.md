# Hoard

Local HuggingFace-compatible mirror and artifact index. Hoard discovers model, dataset and
repository files across local disks, identifies them from their tensor headers, indexes
them in SQLite, and serves them through the HuggingFace Hub API and a dense web UI. Repos
not held locally are proxied from `huggingface.co` and block-cached.

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
cargo build --release
cargo test
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

## Run

```sh
cp hoard.example.toml hoard.toml
cargo run -- serve --config hoard.toml
export HF_ENDPOINT=http://localhost:7099
```
