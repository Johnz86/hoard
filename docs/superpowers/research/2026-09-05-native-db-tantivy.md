# native_db + redb + Tantivy: implementation research

Verified against: `native_db 0.8.2`, `native_db_macro 0.8.2`, `native_model 0.4.20`, `native_model_macro
0.4.20`, `redb 2.6.3`, `bincode 1.3.3`, `tantivy 0.26.1` (plus `tantivy-columnar 0.7.0`,
`tantivy-query-grammar 0.26.0`). Checked 2026-09-05. Vendored sources read from
`~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/`; everything else cites a URL.

## 1. Model evolution

`#[native_model(id = N, version = M, from = OldType)]` is parsed by
`native_model_macro-0.4.20/src/lib.rs:19-67`. Keys: `id`, `version`, `with` (codec override, default
`native_model::bincode_1_3::Bincode`, `lib.rs:30-40`), `from`, `try_from = (Type, ErrorType)`.
`#[native_db]` is a separate derive (`native_db_macro-0.8.2/src/native_db.rs:8-69`) with field attributes
`#[primary_key]` and `#[secondary_key(unique, optional)]` (`model_attributes.rs:85-130`).

```rust
#[derive(Serialize, Deserialize, Debug)]
#[native_model(id = 1, version = 1)]
#[native_db]
struct ItemV1 {
    #[primary_key]
    id: u32,
    name: String,
}

#[derive(Serialize, Deserialize, Debug)]
#[native_model(id = 1, version = 2, from = ItemV1)]
#[native_db]
struct ItemV2 {
    #[primary_key]
    id: u32,
    name_v2: String,
}

impl From<ItemV1> for ItemV2 {
    fn from(v1: ItemV1) -> Self {
        ItemV2 { id: v1.id, name_v2: v1.name }
    }
}
```
(mirrors `native_db-0.8.2/tests/migrate/with_multiple_versions.rs:1-58`.)

`From` runs inside generated code, `decode_upgrade_body.rs:5-50`: if stored version equals target's,
decode directly; if lower, recurse into `from`'s own decode-upgrade and `.into()` forward. Runs
automatically inside `native_model::decode::<T>`; not a background hook.

Load-bearing fact: each `(id, version)` pair gets its own physical redb table. `KeyDefinition::new` builds
`unique_table_name = format!("{}_{}_{}", model_id, model_version, name)` (`key_definition.rs:23-36`).
`get_primary_table` (`r_transaction.rs:26-33`) opens exactly that table for the type you asked for, no
fallback to older versions. `r_transaction().get::<ItemV2>()` sees only rows already in `"1_2_id"`; rows
in `"1_1_id"` stay invisible until moved (`tests/migrate/with_multiple_versions.rs:69-131`). `from =
OldType` makes old rows convertible, not visible, without running `migrate`.

`RwTransaction::migrate::<T>()` and `refresh` are both real, `rw_transaction.rs:292-374` (internal).
`migrate::<T>()` finds the one other same-`id` table with rows (panics if more than one non-empty old
table exists), drains its bytes, decodes each via `native_model::decode::<T>` (running the `From` chain),
inserts into `T`'s table; secondary indexes rebuild via the normal insert path; no-op if nothing to move.
`refresh::<T>()` drains and re-inserts `T`'s own table in place (no move), for re-encoding after a
native_db format upgrade (`upgrade_0_7_x`, `tests/refresh.rs:27-93`), not routine schema bumps. Run all
pending `migrate::<T>()` in one transaction at startup, before serving reads.

**What breaks under bincode 1.3.** The default codec calls plain `bincode::serialize`/`deserialize`
(`codec/bincode_1_3.rs:1-18`) with `.with_fixint_encoding().allow_trailing_bytes()`. `serialize_struct`
discards the struct name and field count (`bincode-1.3.3/src/ser/mod.rs:184-186`), writing fields
positionally with no names or tags.

- Id reuse across unrelated types: `Models::define` only panics when `id` and `version` both collide
  (`models.rs:326-337`). Different structs sharing an `id` at different `version`s are treated as
  sequential versions of one model; `migrate::<T>()` matches an "old" table by `id` alone
  (`rw_transaction.rs:310-313`) and may decode unrelated bytes as `T` (undefined behavior; bincode carries
  no tag).
- Removing a field without bumping version: no field-count/schema-hash check exists; trailing bytes are
  silently allowed, so this can drop data silently or fail with an opaque bincode error.
- Reordering same-width fields without bumping version: silently swaps values into wrong fields, no error;
  different-width reorders corrupt every following field.
- None of this is checked at compile time or registration time beyond the exact `(id, version)` collision
  panic. Bumping `version` and adding `from` on every field-shape change is entirely the programmer's job.

**Codec.** `native_model-0.4.20/Cargo.toml:99-103`: `default = ["serde", "bincode_1_3"]`; native_db 0.8.2
pins `native_model = "=0.4.20"` without disabling defaults, so bincode 1.3 is used unless a model
overrides `with`. `postcard` (`postcard_1_0` feature) and bincode 2 RC (`bincode_2_rc` feature) are opt-in
per-model codecs via `with = native_model::postcard_1_0::PostCard`; both remain positional and
non-self-describing (only the integer wire encoding changes, varint vs. fixed-width). The 8-byte
native_model header (id, version) is written independently of the body codec (`header.rs`,
`wrapper.rs:47-55`).

## 2. Keys

`#[secondary_key(unique)]` (`model_attributes.rs:100-127`): `util_insert_secondary_keys`
(`rw_transaction.rs:129-159`) checks the secondary multimap for an existing primary key under that value;
if unique and non-empty, returns `Error::DuplicateKey { key_name }` (same variant as primary-key
duplicates, `error.rs:72-73`). `get().secondary` against a non-unique key returns
`Error::SecondaryKeyConstraintMismatch`.

`optional` on `Option<T>`: produces `KeyEntry::Optional(Option<Key>)`; on insert, `None` simply
`continue`s (`rw_transaction.rs:132-141`) so the record is omitted from that secondary index, not indexed
under a sentinel. `concrete_remove` mirrors this.

Composite keys use the crate's `ToKey` trait:
```rust
pub trait ToKey: Debug {
    fn to_key(&self) -> Key;
    fn key_names() -> Vec<String>;
}
```
(`db_type/key/key.rs:177-180`). Tuples up to 12 elements have built-in impls concatenating each element's
bytes in order (`key.rs:235-334`), so a method-key returning a tuple (e.g.
`#[native_db(secondary_key(full_key -> (String, u32)))]`) is the simplest composite key. A hand-rolled
type (`tests/custom_type/custom.rs:9-20`):
```rust
struct LastFirst(String, String);
impl ToKey for LastFirst {
    fn to_key(&self) -> Key {
        let mut bytes = self.0.as_bytes().to_vec();
        bytes.push(0); // delimiter, mirrors composite_key() in key_definition.rs:75-83
        bytes.extend_from_slice(self.1.as_bytes());
        Key::new(bytes)
    }
    fn key_names() -> Vec<String> { vec!["LastFirst".to_string()] }
}
```
Redb key ordering is byte-lexicographic (`impl RedbKey for Key { fn compare(a, b) -> Ordering { a.cmp(b) }
}`, `key.rs:455-458`); primitives encode via `to_be_bytes()` (`key.rs`, near line 399), so unsigned byte
order matches numeric order (load-bearing for section 5).

Secondary scans (`transaction/query/scan/secondary_scan.rs`): `all()` (:77-92), `range<R: RangeBounds<impl
ToKey>>(range)` (:128-151), `start_with(impl ToKey)` (:187-214). No `.equal(...)`: use
`range(v.clone()..=v)`/`start_with` for a non-unique equality lookup, `RGet::secondary` for a unique one.
`PrimaryScan` offers `all()`/`range()` only; both scan iterators implement `DoubleEndedIterator`, so
`.next_back()` gets the last-ordered row without a full scan. `RwDrain::primary::<T>()` exists but its doc
says "TODO: needs to be improved, so don't use it yet" (`drain.rs:8-20`); `RwDrain::secondary` is an
unimplemented `todo!()` (`drain.rs:22-27`). `RLen`/`RwLen` expose
`primary::<T>()`/`secondary::<T>(key_def) -> Result<u64>` via `redb::ReadableTableMetadata::len()`.

`native_db::Database<'a>` holds a direct `redb::Database` (no extra `Arc`/`Mutex`), a `HashMap` of table
definitions, `Arc<RwLock<Watchers>>`, `AtomicU64` (`database.rs:34-40`); no `unsafe impl Send`/`Sync`
anywhere, thread-safety is auto-derived. `redb::Database { mem: Arc<TransactionalMemory>,
transaction_tracker: Arc<TransactionTracker> }` (`redb-2.6.3/src/db.rs:331-334`) is Arc-wrapped
throughout, with `TransactionTracker` using `Mutex<State>` plus `Condvar`
(`transaction_tracker.rs:97-100`): Send+Sync by construction. redb's own doc: "Multiple reads may be
performed concurrently, with each other, and with writes. Only a single write may be in progress at a
time." (`db.rs:306-307`). native_db adds no extra serialization around `begin_write`/`begin_read`, so an
`r_transaction()` on one thread while another holds an open `rw_transaction()` is redb's supported MVCC
case: the reader sees a consistent pre-write snapshot. Wrap `Database` in `Arc` to share across threads.

## 3. redb internals

**Durability** (`redb-2.6.3/src/transactions.rs:356-371`, `#[non_exhaustive]`): `None`, no fsync, pages
free only on a later higher-durability commit ("rapid growth of the database file"); `Eventual`, on
non-macOS Unix still calls `File::sync_data()` unconditionally (`cached_file.rs:296-299`), macOS uses
lighter `fcntl(F_BARRIERFSYNC)`; `Immediate`, guaranteed persistent on `commit()` return, forces
`sync_data()` everywhere; `Paranoid` (deprecated 2.3.0), same as `Immediate` plus 2-phase commit.
`WriteTransaction::new` sets `Immediate` unconditionally (`transactions.rs:835`); doc: "Defaults to
`Durability::Immediate`."

**native_db exposes none of it.** `grep -rin "durability" native_db-0.8.2/src` is empty; every
`begin_write()` call site uses the stock transaction. Every commit runs at redb's default `Immediate`: a
synchronous fsync per commit. Declined upstream: issue
[native_db#40](https://github.com/vincent-herlemont/native_db/issues/40) lists `set_durability` among
seven redb methods requested; maintainer cited no current use case.

**Savepoints and `compact()`.** redb has a full savepoint API on `WriteTransaction`:
`persistent_savepoint`, `get_persistent_savepoint`, `delete_persistent_savepoint`,
`list_persistent_savepoints`, `ephemeral_savepoint`, `restore_savepoint` (`transactions.rs:1060-1216`;
persistent savepoints require `Immediate`). native_db exposes none (same #40). `redb::Database::compact()
-> Result<bool, CompactionError>` (`db.rs:501`) refuses if any read transaction is live or any savepoint
exists. native_db exposes `compact(&mut self) -> Result<bool>` (`database.rs:154-160`) but always returns
`Ok(true)`, discarding redb's real boolean.

**Long-lived reads and file growth.** redb is copy-on-write MVCC; `durable_commit` computes
`free_until_transaction = oldest_live_read_transaction().map_or(current, |x| x.next())` before freeing
pages (`transactions.rs:1931-1938`). A scan holding one read transaction open pins the free list:
concurrent writes allocate pages that cannot be reclaimed until the scan closes (and `compact()` cannot
run while it's open). File size grows with write volume during a long scan; reopen read transactions
periodically to avoid this.

**Large write transaction memory.** No mmap backend (removed in
[redb#536](https://github.com/cberner/redb/pull/536), 2023-03-24, for provable safety). Writes go through
`PagedCachedFile`, a bounded LRU read cache plus write buffer; `set_cache_size` splits the budget 90/10,
default 1 GiB total (~100 MiB write-buffer cap). Once buffered dirty pages exceed that cap, redb evicts
and writes to disk mid-transaction, before `commit()`. A transaction inserting tens of thousands of rows
is bounded by the cache size, not transaction size. No open redb issue matching "OOM"/"large
transaction"/"memory grows" found as of 2026-09-05.

**File locking.** `FileBackend::new` (`file_backend/unix.rs:32-45`) calls raw `libc::flock(fd, LOCK_EX |
LOCK_NB)`; a second process gets `EWOULDBLOCK`, mapped to `DatabaseError::DatabaseAlreadyOpen`, released
on `Drop`. native_db inherits this unmodified, wrapped as `RedbDatabaseError(DatabaseAlreadyOpen)`.

**Newer redb and native_db's adoption.** redb 3.0.0 (2025-08-09) added "Read-only multi-process support":
`ReadOnlyDatabase` and `Builder::open_read_only()`, shared file lock; 2.6.3 (a 2.x patch, 2025-08-23)
lacks it. An unreleased, feature-gated `experimental-multiprocess` (targeting a future 5.0.0) proposes
`set_concurrency_mode()` with `SingleWriterProcess`/`MultiWriterProcess`. native_db's main branch
`Cargo.toml` already pins `redb = "=3.0.2"` (up from 0.8.2's `redb = "2.1.4"`), via
[#429](https://github.com/vincent-herlemont/native_db/pull/429)/
[#437](https://github.com/vincent-herlemont/native_db/pull/437) (Renovate bumps, merged 2025-08-18), but
has not adopted the read-only capability: zero matches for `read_only`, `ReadOnly`, `open_read_only`,
`concurrency_mode` on main; `Builder` still calls stock `.create()`/`.open()`. No open issue proposes
adopting it. Sat unreleased over a year.

## 4. Tantivy 0.26.1

**Reload policy.** `ReloadPolicy` has two variants: `Manual`, `OnCommitWithDelay` (builder default). Not a
commit-completion channel: for `MmapDirectory`, `watch()` delegates to `FileWatcher::watch`, spawning a
thread that polls `meta.json`'s CRC32 every `POLLING_INTERVAL` (500ms, 1ms under `cfg(test)`) and reloads
only on change. "Near real time" means "up to roughly one polling interval after `meta.json` changes," via
polling. `Manual` requires calling `reader.reload()` explicitly.
```rust
let reader = index.reader_builder()
    .reload_policy(ReloadPolicy::OnCommitWithDelay)
    .try_into()?;
```

**Delete-by-term plus re-add.** `IndexWriter::delete_term(term) -> Opstamp` queues a `DeleteOperation`
tagged with an opstamp; visible only after `commit()`, like adds. Deletes and adds order by opstamp, so
`delete_term(t)` then `add_document(doc_with_t)` before `commit()` is the standard update idiom.

**Commit cost.** `commit()` is `prepare_commit()?.commit()`. `prepare_commit()` joins all indexing worker
threads. Doc: "this operation is not at all really light." The path applies queued deletes to segment
bitsets, promotes segments, writes `meta.json` via an fsync'd temp-file-then-atomic-rename plus a
directory fsync, garbage-collects files, may schedule merges. Batch commits (hundreds/thousands of docs);
never commit per record.

**Facet vs. fast-field aggregation.** `Facet` is a `/`-delimited hierarchical path; `FacetCollector`
forbids adding two facets where one is an ancestor of the other, and its `FacetCounts` fruit gives
direct-children counts plus heap-based `top_k` in one pass: purpose-built hierarchy, minimal allocation. A
plain fast field (`FAST`) plus `TermsAggregation` gives flat buckets with no rollup, but composes with
metric sub-aggregations (min/max/avg/percentiles), `min_doc_count`/`size` controls, and
Elasticsearch-style JSON. Use `Facet` for genuine hierarchy, aggregations for flat counts plus metrics.

**Aggregation API.** `TermsAggregation`: `field: String`, `size: Option<u32>` (default top 10),
`segment_size`/`shard_size` accuracy knob, `min_doc_count` (`0` is expensive, returns every term).
`MinAggregation`/`MaxAggregation`: `{ field: String, missing: Option<f64> }` via `from_field_name`. Run
via `AggregationCollector::from_aggs(aggs, AggContextParams::default())`:
```rust
let request = r#"{
  "by_country": { "terms": { "field": "country", "size": 20 } },
  "min_price":  { "min":   { "field": "price" } }
}"#;
let aggs: Aggregations = serde_json::from_str(request)?;
let collector = AggregationCollector::from_aggs(aggs, AggContextParams::default());
let result = searcher.search(&query, &collector)?;
```

**`TopDocs::order_by_fast_field`.** Accepts only `FastValue` types: `u64`, `i64`, `f64`, `bool`,
`DateTime` (not strings; separate `order_by_string_fast_field`). Field must be `FAST` or `check_schema`
errors. Multivalued fields use only the first value; missing values sort as `None`.
```rust
let top = TopDocs::with_limit(2).order_by_fast_field::<i64>("revenue", Order::Desc);
let rows: Vec<(Option<i64>, DocAddress)> = searcher.search(&query, &top)?;
```

**Custom `Collector`.** `Collector: Sync + Send` with `type Fruit: Send + Downcast`, `type Child:
SegmentCollector`; `for_segment` builds one `Child` per segment (parallel, order unspecified);
`merge_fruits(Vec<Child::Fruit>) -> Fruit` is the cross-segment reduction point; `requires_scoring()` lets
the executor skip scoring when unneeded. `SegmentCollector: 'static` with `collect(doc, score)` and
`harvest() -> Fruit`.

**Doc store compression.** Default features include `lz4-compression` (`lz4_flex`) alongside
`columnar-zstd-compression` (fast-field columnar storage only, not the doc store). `Compressor { None, Lz4
(feature-gated), Zstd (feature-gated, with level) }`, no Snappy variant in 0.26.1. Default prefers `Lz4`,
else `Zstd`, else `None`. Without `lz4-compression`, deserializing `"lz4"` from index settings errors
naming the feature.

**`QueryParser` vs. hand-built `BooleanQuery`.** `QueryParser` disables regex by default (opt in via
`allow_regexes()`); only prefix construct is phrase-prefix (`"..."*`), no general wildcards; phrase slop
only via `~N`; always compiles to one flat `Occur`-tagged clause list per grammar production, with no
custom `Query` impls, arbitrary per-clause boost mixed with other occurs, or independent
minimum-should-match at nested levels. `BooleanQuery::new(Vec<(Occur, Box<dyn Query>)>)` takes any boxed
`Query` with any `Occur`, nested by boxing another `BooleanQuery`: the target for a custom grammar
compiling to `RangeQuery`/`FuzzyTermQuery`/`BoostQuery`/custom scorers with exact structural control.
Convenience constructors `intersection()`/`union()`/`union_with_minimum_required_clauses()` cover common
shapes; `new()` is unrestricted.

## 5. Outbox pattern on native_db

**Model.** `SearchChange { seq: u64, artifact_id: ArtifactId, kind: ChangeKind }` with `#[primary_key]
seq: u64`, inserted in the same transaction as the catalog write:
```rust
let rw = db.rw_transaction()?;
rw.upsert(artifact)?;
rw.insert(SearchChange { seq: next_seq, artifact_id: artifact.id, kind: ChangeKind::Upsert })?;
rw.commit()?;
```
Because `commit()` runs at redb's default `Durability::Immediate` (section 3), both writes become durable
together with one fsync, or neither does.

**Key type for ordered scans.** Make `seq` the primary key, typed `u64`. Primary scans are already ordered
by the primary key's bytes, and `ToKey`'s primitive impls encode via `to_be_bytes()` while redb's
`Key::compare` is plain `a.cmp(b)` byte-lexicographic comparison; big-endian unsigned integers make byte
order match numeric order, so `.scan().primary::<SearchChange>()?.range(cursor+1..)` yields true creation
order with no secondary index. A signed or `to_le_bytes()`-encoded type would not sort correctly.

**Allocating the sequence without a global counter.** redb allows only one write transaction at a time
(`TransactionTracker` serializes writers via `Mutex` plus `Condvar`), so a read-then-insert of the next
sequence value inside the same transaction that writes the outbox row is race-free with no separate atomic
counter:
```rust
let next_seq = rw.scan().primary::<SearchChange>()?
    .all()?
    .next_back()               // DoubleEndedIterator: highest existing key
    .transpose()?
    .map_or(1u64, |last| last.seq + 1);
```
Do not use `RwLen::primary::<SearchChange>()` here: `len()` reflects live row count, and once drained rows
are deleted, `len() + 1` would collide with sequence numbers already consumed and gone. Reverse-scanning
reads the actual last key, staying correct across deletions.

**Idempotent replay after a crash.** Keep a durable cursor record (fixed primary key, e.g. `()` or a
constant `0u8`) holding `last_drained_seq`. Drain loop: read the cursor,
`scan().primary::<SearchChange>()?.range(cursor+1..)?` for pending changes, apply each to Tantivy via
`delete_term` then `add_document` (idempotent: replaying the same document write twice yields the same end
state), `writer.commit()`, and only after that commit succeeds, advance and persist `last_drained_seq` in
a fresh `rw_transaction`. A crash between the Tantivy commit and the cursor update just redoes idempotent
work on restart; a crash before it leaves the cursor unmoved and the range retried from scratch. Because
native_db commits are immediately durable, the outbox rows and cursor can never be silently lost between
"the catalog write happened" and "the drain thread saw it." Prune drained rows in a separate transaction
after the cursor advances (remove by primary key one at a time; `RwDrain::primary` is not production-ready
in 0.8.2) to keep the outbox table small.

## 6. Project health

**native_db** (pinned 0.8.2). Release cadence stalled: crates.io shows 0.5.0 (2023-12-18) through 0.8.2
(2025-07-08), nothing published in roughly 14 months since as of 2026-09-05
([versions](https://crates.io/crates/native_db/versions)). 0.8.2's notes call it a maintenance-branch cut.
Last commit to `main` 2025-10-10, but recent history is almost entirely Renovate bumps; last substantive
human work was a redb-v3 migration cluster, 2025-07-01 to 2025-09-17. main's unreleased `Cargo.toml`
already depends on `native_model = "0.6.2"` and `redb = "=3.0.2"`, while shipped 0.8.2 pins `native_model
= "=0.4.20"` and `redb = "2.1.4"`: unreleased over a year. No MSRV declared at the 0.8.2 tag or on main.
Relevant open issues: [#462](https://github.com/vincent-herlemont/native_db/issues/462), a migration panic
(`MigrateLegacyModel`) with more than one model needing migration (open since 2026-04-21, no maintainer
response); [#357](https://github.com/vincent-herlemont/native_db/issues/357), the stale native_model
0.4.20 to 0.6.1 upgrade (open since 2025-04-04); and
[#469](https://github.com/vincent-herlemont/native_db/issues/469), a `range(..=end)` scan missing an item
whose key equals `end`. Searches for "corruption," "data loss," "large transaction," "out of memory"
returned no matches; none found for `Option<T>` keys either. README, verbatim: "Active development. The
API is not stable yet and may change in the future"
([README](https://github.com/vincent-herlemont/native_db/blob/main/README.md)).

**native_model** (pinned 0.4.20). Similarly stalled: 0.4.x through 0.4.20 (2024-10-05), then
0.5.0/0.6.0/0.6.1 within two days in November 2024, then 0.6.2 (2025-06-19), nothing since
([versions](https://crates.io/crates/native_model/versions)). Last commit 2026-08-16, but last functional
change was 2025-05-17/18. main declares `rust-version = "1.73.0"`, but the 0.4.20 tag in use has no MSRV
field. Of its nominally-10 open issues, only four are real (docs requests, a PGO idea, the Renovate
dashboard meta-issue); none found for data loss, corruption, migration/upgrade failures, or versioning
bugs. Its README: "Early development. Not ready for production."
([README](https://github.com/vincent-herlemont/native_model/blob/main/README.md)).

## Implications for Hoard

- Run every pending `migrate::<T>()` in one transaction at startup, before serving reads: old-version rows
  are physically invisible to the new type's `get`/`scan` until moved (section 1). Test it directly:
  insert under vN, open with vN+1 registered, assert absence before migrate and presence after.
- Bump `version` and add `from = OldType` on any field add/remove/reorder/type change, and never reuse an
  `id` across unrelated structs: bincode 1.3 has no schema check and `Models::define`/`migrate` will not
  catch either mistake, so a missed bump or a reused id can silently corrupt, misread, or cross-decode
  data (section 1).
- Design the outbox's `seq` as a `u64` primary key, allocated by reverse-scanning (`.next_back()`) inside
  the same write transaction as the record it logs, never via `len()` (section 5); redb's single-writer
  serialization makes this race-free without a global atomic counter.
- Persist a drain cursor and commit to Tantivy before advancing it, so a crash replays a bounded
  idempotent (`delete_term` plus re-add) tail instead of losing or double-processing changes (section 5).
- Every native_db commit is an unconditional fsync (`Durability::Immediate`, no override exposed): batch
  small, frequent writes into fewer, larger transactions; there is no cheaper durability knob in 0.8.2
  (section 3).
- Do not hold a read transaction open across a long scan while writes happen concurrently: it pins redb's
  free list and grows the file; reopen read transactions periodically instead (section 3).
- Batch Tantivy writes: `commit()` joins indexing threads and does two fsyncs plus GC, so drain the outbox
  in batches rather than committing per change; use `OnCommitWithDelay` (polling, roughly 500ms) for the
  search reader, not `Manual`, unless the UI needs a forced reload after a known write (section 4).
- Prefer `Facet` fields only for genuine hierarchy; use a plain fast field with `TermsAggregation` for
  flat facet counts, and reserve hand-built `BooleanQuery` construction for Hoard's own grammar rather
  than fighting `QueryParser`'s fixed clause shape and disabled-by-default regex/wildcard support (section
  4).
- Track native_db's unreleased redb-3 migration (main already pins `redb = "=3.0.2"`, unreleased over a
  year): it brings `ReadOnlyDatabase` within reach, but native_db has not adopted it and no issue proposes
  it; do not plan multi-process read access until it lands (sections 3, 6).
- Treat both crates as low-cadence and explicitly unstable/pre-production per their own maintainers: pin
  exact versions, isolate native_db/native_model types behind Hoard's own catalog traits as the decision
  doc already plans, and budget time to track or patch around known open bugs (native_db #462, a migration
  panic with multiple pending migrations, and #357, native_model stuck at 0.4.20) rather than expecting
  timely upstream fixes (section 6).
