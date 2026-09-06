I would pick native_db backed by redb, with Tantivy as the separate rebuildable search index.

For Hoard’s likely deployment scale—laptops and small servers holding several terabytes of model files—this is the best total-cost choice. Fjall might win a narrowly defined high-ingest benchmark, but Hoard’s metadata volume is far smaller than
its managed file volume, while the engineering cost of correctly building a database layer over raw Fjall would be substantial.

## Why storage capacity does not imply a huge database

On this machine:

- Storage: 3.6 TB NVMe/ext4.
- Used: 2.3 TB.
- data/hoard.db: 8.15 MiB.
- Current catalog:
    - 943 entries
    - 731 blobs
    - 231 artifacts
    - 136 repositories
    - 1,252 evidence records

- Approximately 3.8 MiB of the SQLite file is reusable free pages.
- Live database content is roughly 4.3 MiB.
- Tensor summaries and FTS consume most of that—not the relational catalog.

A crude linear projection gives roughly:

Files/entries    Estimated catalog scale
━━━━━━━━━━━━━━━  ━━━━━━━━━━━━━━━━━━━━━━━━━
        100,000            Hundreds of MiB
───────────────  ─────────────────────────
    1,000,000            Roughly 4–8 GiB
───────────────  ─────────────────────────
    10,000,000                Tens of GiB

Those are intentionally conservative estimates. Separating FTS into Tantivy and compacting serialization should improve them.

Even a machine containing several terabytes of models probably has thousands or hundreds of thousands of files, not tens of millions. Both redb and Fjall can comfortably handle the resulting metadata. Raw engine throughput is therefore unlikely
to be the binding constraint.

## Why I would choose native_db

### 1. It already solves the expensive correctness work

native_db provides:

- Typed Rust records.
- Primary indexes.
- Unique and non-unique secondary indexes.
- Composite keys.
- Ordered scans and prefix lookups.
- Atomic transactions.
- Automatic secondary-index maintenance.
- Model migrations.
- Snapshots and integrity checking.
- Configurable serialization.

Its transactions are backed by redb, a stable, pure-Rust, ACID, MVCC B+tree engine. The current native_db transaction API supports insert, update, upsert, remove, primary and secondary lookup, scans, migrations and commits (transaction API).

Those features directly replace much of SQLite’s current responsibility.

### 2. Fjall would make Hoard responsible for too much

With raw Fjall, we would need to implement and maintain:

- Record serialization and version evolution.
- Primary-key layout.
- Monotonic ID allocation.
- Unique indexes.
- Non-unique indexes.
- Composite index encoding.
- Removal of obsolete index entries during updates.
- Referential validation.
- Cascading deletion.
- Orphan collection.
- Materialized counters.
- Index reconstruction.
- Integrity checking.
- Migration tooling.
- Backup and restore conventions.
- Corruption diagnostics.
- Transaction retry and durability policy.
- Tests proving that every mutation maintains every index.

The primary danger is not writing these initially. It is maintaining them through future schema evolution.

For example, changing an entry’s artifact would need one atomic operation that:

1. Loads the old entry.
2. Removes the old artifact-to-entry index key.
3. Inserts the new artifact-to-entry index key.
4. Updates the entry record.
5. Updates affected blob path counts.
6. Marks both affected search documents dirty.
7. Possibly removes orphaned artifacts or blobs.

A forgotten index mutation could silently return incomplete search results months later. SQLite currently prevents much of this through constraints and indexes; native_db handles the basic index maintenance automatically.

### 3. B+tree storage fits Hoard well

Hoard’s storage access is dominated by:

- Point lookups.
- Composite unique lookups.
- Ordered ID scans.
- Small range scans.
- Small and medium transactions.
- Occasional large scan-ingestion batches.
- Many reads between scan runs.

That is a good B+tree workload. Fjall’s LSM architecture is most attractive when sustained write throughput, sequential ingestion and write amplification trade-offs dominate.

Fjall’s recommended leveled compaction targets approximately 1.1× space amplification, but explicitly trades that for higher write amplification (Fjall compaction documentation). It also introduces operational variables such as:

- Memtable sizing.
- Block-cache sizing.
- Compaction scheduling.
- Write stalls.
- Tombstone accumulation.
- Temporary disk use during compaction.
- Read amplification across levels.

Those are manageable, but they are additional maintenance costs with little likely benefit at Hoard’s metadata scale.

### 4. Single-file redb storage is convenient for laptops

A redb-backed catalog gives Hoard a simple layout:

data/
    catalog.redb
    search/
        Tantivy segments
    repos/
    profiles/

Advantages include:

- Straightforward catalog backup.
- No background database compaction threads.
- Easy detection of unexpectedly growing files.
- Fewer database files mixed with large model storage.
- Predictable behavior on laptop suspend/resume.
- Easier support diagnostics.
- Pure-Rust builds.

Tantivy is disposable and can be rebuilt. catalog.redb contains the authoritative user state and catalog generation data.

## Main drawback of native_db

Its own README states that the API is still unstable and under active development (native_db repository). It is not as mature as SQLite, redb or Fjall.

That does not change my choice, but I would contain the risk:

hoard-index
    domain/
        ArtifactRecord
        BlobRecord
        EntryRecord
        ...
    catalog/
        Catalog
        ReadTransaction
        WriteTransaction
    backend/
        native_db/
    search/
        Tantivy

No crate outside hoard-index should import native_db types. All interaction should go through Hoard-owned traits and domain records.

If native_db changes incompatibly or becomes unmaintained, there are three escape paths:

1. Pin the known-good version.
2. Maintain a small fork.
3. Replace the adapter with direct redb or Fjall without rewriting callers.

Maintaining a thin fork of native_db would still probably cost less than designing and validating Hoard’s complete secondary-index system over Fjall.

## Where Fjall would become the better choice

I would switch the recommendation to Fjall if measurements demonstrate one or more of these:

- Tens of millions of catalog entries.
- Continuous high-rate writes rather than periodic scans.
- native_db transactions consume excessive memory during bulk scans.
- redb file growth or copy-on-write amplification becomes material.
- Scan ingestion cannot reach an acceptable target even with batching.
- Hoard requires custom prefix-compressed indexes that native_db cannot express.
- We want to merge telemetry retention and automatic expiry into compaction filters.
- The project is willing to own a storage subsystem as a major architectural component.

Fjall is technically strong and actively developed, with safe Rust, cross-keyspace transactions, range scans, compression and automatic maintenance (Fjall repository). I simply do not think Hoard currently needs enough of its advantages to pay
the ownership cost.

## Recommended durability split

With native_db/redb:

Data                                                       Durability
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Overrides, noise decisions, shelves, loadouts, settings    Immediate
─────────────────────────────────────────────────────────  ──────────────────────────────────────────────────
Catalog scan batches                                       Immediate per completed batch
─────────────────────────────────────────────────────────  ──────────────────────────────────────────────────
Peer digest application and cursor                         Same atomic transaction
─────────────────────────────────────────────────────────  ──────────────────────────────────────────────────
Projection records                                         Immediate, followed by filesystem reconciliation
─────────────────────────────────────────────────────────  ──────────────────────────────────────────────────
Usage events                                               Batched if acceptable
─────────────────────────────────────────────────────────  ──────────────────────────────────────────────────
Job progress                                               Relaxed/batched
─────────────────────────────────────────────────────────  ──────────────────────────────────────────────────
Tantivy search changes                                     Eventual and replayable

Search updates should be driven by a persistent outbox/change sequence stored inside the same native_db transaction as the catalog change.

## Final architecture

native_db / redb
    authoritative typed catalog
    durable user data
    relationships expressed by IDs
    automatic primary and secondary indexes
    transactional search-change outbox

Tantivy
    one document per artifact
    filtering
    full-text/prefix search
    facets and counts
    sorting and result pagination
    completely rebuildable

Parquet, optionally
    export
    diagnostics
    offline analysis
    portable snapshots of derived catalog data

## Bottom line

For this project, on these machines, I would optimize for correctness and maintenance:

> Choose native_db, isolate it behind a Hoard-owned catalog API, and use Tantivy for search.
