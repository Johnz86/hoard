# Hoard

Local HuggingFace-compatible mirror and artifact index. Hoard discovers model, dataset and
repository files across local disks, identifies them from their tensor headers, and indexes
them in a native catalog: a redb database of typed records with a Tantivy search index
rebuildable from those records alone. It serves that index through the HuggingFace Hub API,
git over HTTP and a dense web UI, proxying and block-caching `huggingface.co` for repos it
does not hold locally. Nodes find each other on the LAN, gossip their index and pull
artifacts peer to peer; loadouts pin a checkpoint plus its adapters and auxiliaries as a
portable text file, applied to a node as symlinks.

- `docs/use-case.md`: designer's brief
- `docs/feature-plan.md`: scope, phases, acceptance criteria
- `docs/architecture.md`: stack, crate layout, data flow
- `docs/native-catalog.md`: the catalog's layout, modules, grammar and measured numbers
- `docs/handoff.md`: repository state, how the agentic loop is run, outstanding work
- `docs/api/hub-compat.md`: HuggingFace Hub routes and error contract
- `docs/api/hoard-api.md`: `/api/v1` JSON API for the UI and CLI
- `docs/design/ui-spec.md`: UI tokens, layout and keyboard map from the design handoff
- `docs/design/handoff/`: the Claude Design export (open `project/Hoard App.dc.html`)

## Agentic development

`CLAUDE.md` imports `AGENTS.md`, points at the docs above and states the compatibility
policy. `.claude/` holds the project settings (permission allowlist, rustfmt hook, enabled
plugins), path-scoped rules for `crates/` and `web/`, seven skills and three agents.

Work runs as waves: write a plan with `/hoard-write-plan`, dispatch the `wave-implementer`
agent with the plan path and the commit trailers, run `/hoard-verify`, then dispatch
`perf-reviewer` or `rust-reviewer`. `/hoard-next-phase` starts a phase from
`docs/feature-plan.md`; `/hoard-catalog`, `/hoard-measure` and `/hoard-perf-review` carry the
catalog knowledge, the measurement protocol and the performance checklist. `docs/handoff.md`
describes the loop in full.

## Build

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

## Run

```sh
cargo run -- serve --bind 127.0.0.1:7099
```

No configuration file or example sources are required. Open `http://127.0.0.1:7099/`;
an empty installation opens Sources with guidance for adding your first folder. This command
listens locally. See [first-time setup](docs/first-run.md) for adding resources and enabling
network sharing, or [watch the narrated introduction](docs/introduction-video.md).
Use `--config hoard.toml` for an explicit configuration; `hoard.example.toml` documents the
options. Clients can set `HF_ENDPOINT` to the node's reachable address.

The Sources page can add folders while the server runs. Their paths refer to the server's
filesystem. Hoard saves them in `<data_dir>/sources.json` and loads them alongside the
`[[index.sources]]` entries in `hoard.toml` on the next start.

See [design status and next work](docs/design/README.md) for the redesign review and the
current interaction and verification notes.

Git projects share the Index table/cards, cursor and inspector with artifacts. Search by
project name, committed README text or file path; click to inspect, double-click or Enter
to open. Project details reuse the standard header, tabs and right rail for README, files,
refs and clone/share actions. Sources offers BROWSE for any folder as a normal source query.
COPY CLONE uses the
Hoard server's read-only Git endpoint; COPY PROJECT LINK shares the current revision and file.
Git projects do not need model artifacts or triage before they can be found and shared.

Sources offers REMOVE, then CONFIRM or CANCEL, on each indexed folder. Removal stops indexing
and sharing that source, preserves files on disk and survives restart, including folders from
`hoard.toml`. Add the folder again to restore it. Finished scans can be dismissed individually
or cleared from Activity; this only removes history.

For other machines to use those links, configure `[server] bind` and `public_url` with a
reachable LAN address, as in `hoard.example.toml`. A loopback address works on its own machine.
