# Documentation

Use the guide for your role. These pages describe supported behavior; implementation history
and superseded plans belong in Git, not in the current documentation.

## Use Hoard

- [Getting started](user-guide/getting-started.md): start empty, add folders, find and share resources.
- [Introduction video](user-guide/introduction.md): narrated walkthrough, captions and transcript.
- [Configuration](user-guide/configuration.md): settings, persistence and access boundaries.
- [Deployment and maintenance](operations/deployment.md): listeners, backups, upgrades and diagnostics.

- [Model collections](user-guide/loadouts.md): group selected models and download them to a client.

## Understand the system

- [Product scope](design/product-scope.md): supported workflows and intentional limits.
- [UI specification](design/ui-spec.md): current layouts, terminology and keyboard behavior.
- [Architecture](reference/architecture.md): crate boundaries, runtime stack and data flow.
- [Native catalog](reference/native-catalog.md): persistence, writes, indexing and recovery.
- [Source layouts](reference/source-layouts.md): folder aliases and placement versus inferred roles.
- [Hoard API](reference/api/hoard.md): the JSON contract used by the application.
- [Hugging Face compatibility](reference/api/hugging-face.md): Hub routes and protocol behavior.
- [Catalog storage decision](decisions/native-catalog.md): rationale and operational consequences.
- [Performance evidence](performance/README.md): dated measurements and reproduction policy.

## Contribute and run tasks

- [Contributing](../CONTRIBUTING.md): development and verification.
- [UI change review](contributing/ui-changes.md): baselines and broader application acceptance.
- [Task catalog](../tasks/README.md): documentation checks, first-run verification, recording and benchmarks.

Each behavior has one owning reference. Update that page with the implementation rather than
adding a delivery log. Mark measurements with their commit and workload; do not present them
as current production guarantees. See the [documentation maintenance rules](../CONTRIBUTING.md#documentation).
