# Hoard

Find, review and share models, datasets and Git projects already on your disks. Hoard indexes
folders in place and serves resources through a searchable web UI, Git over HTTP and a
Hugging Face-compatible API. Files stay where they are.

## Get started

```sh
cargo build --release --workspace -j 2
./target/release/hoard serve --bind 127.0.0.1:7099
```

Open `http://127.0.0.1:7099/`. No configuration file or example folders are required.
An empty installation opens Sources, where you can add your first folder and review sharing
settings. This command listens locally; network sharing requires a reachable address and listener.

- [Getting started](docs/user-guide/getting-started.md)
- [Watch the introduction](docs/user-guide/introduction.md)
- [Configuration](docs/user-guide/configuration.md)
- [Deployment and maintenance](docs/operations/deployment.md)
- [Documentation index](docs/README.md)

## What you can do

- Search models, Git projects and named loadouts in one Index, with facets, shelves and an inspector.
- Read committed project documentation, inspect files and revisions, and copy clone commands.
- Find models by nested path or filename alias and inspect their selected source copy.
- Group models into loadouts and prepare client downloads with editable destination paths.
- Add or remove indexed folders without moving or deleting their original files.
- Discover peers, exchange resource indexes and acquire resources over the network.

Hoard is pre-release software (currently 0.1.0). Catalog format changes can rebuild local
catalog state; review the [upgrade and backup guidance](docs/operations/deployment.md#upgrades-and-backups)
before operating a persistent node. A node write token restricts changes, while resource
browsing and downloads remain public to clients that can reach the listener.

## Develop and maintain

See [CONTRIBUTING.md](CONTRIBUTING.md) for verification, code ownership and documentation
standards. Repeatable workflows live in [task directories](tasks/README.md), each with its
own instructions, executable entry point and required inputs.
