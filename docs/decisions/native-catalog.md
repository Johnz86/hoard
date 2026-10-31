# Native catalog and derived search index

## Context

Hoard needs transactional metadata, content identities, source/revision relationships,
user-maintained shelves and loadouts, and search with facets and directional compatibility.
The canonical metadata store and the search representation have different responsibilities.

## Decision

Use native_db over redb for typed records and secondary indexes. Use Tantivy as a derived
search index that can be rebuilt from catalog records without rescanning resource files.
Each entry contributes a search document; per-artifact collapse and preferred-path selection
preserve resource semantics when several paths or nodes name the same bytes.

Entry writers update records, derived counters and the search outbox in the same transaction.
Search refresh consumes that outbox. A rebuild publishes a new directory only after it is
complete. Persisted formats follow the [catalog compatibility policy](../../CLAUDE.md#compatibility).

## Consequences

One process owns a catalog at a time. Search is recoverable independently of the canonical
records. Rebuilding from resource files does not recreate user-maintained catalog state;
operators need backups. Native_db secondary scans materialize matching primary keys, and
write transactions retain serialized writes until commit, so batching and measurement matter.

See [the catalog reference](../reference/native-catalog.md) for the implementation contracts
and [dated measurements](../performance/catalog-measurements.md) for evidence on specific workloads.
