# What dense page packing would take

A design, not a change. It answers the "pack the pages denser" bullet of
`docs/native-catalog.md` "Remaining work" with what redb actually does, what a bulk load or a
dense rewrite would buy, and the measurement that would settle it.

Verified against `redb 2.6.3` and `native_db 0.8.2`, read from
`~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/redb-2.6.3/` and the clone at
`/home/johnz86/GIT/native_db`. Checked 2026-09-07. The catalog numbers are the ones
`docs/native-catalog.md` records at `1783f2b`, 1M entries, after the rebuild.

## 1. The prize

Across every table, intra-page slack is 608,835,673 bytes, 45.4 per cent of `catalog.redb`,
and the four record tables sit at 42 to 55 per cent leaf fill. Perfect packing would take the
seven surviving tables from 1,329,451,008 bytes of pages to roughly 700,000,000. Nothing else
on the remaining-work list is worth half that, which is why the bullet exists.

## 2. What decides leaf fill

A leaf is one page. `LeafBuilder::should_split` (`src/tree_store/btree_base.rs:557-563`)
splits when the rebuilt leaf would need more than `get_page_size()` bytes and holds more than
one pair. `LeafBuilder::build_split` (`btree_base.rs:565-615`) then walks the pairs
accumulating key and value bytes and cuts at the first pair whose running total reaches
`total_size / 2`: **the split is always at the byte midpoint**, and both halves land in a page
of their own.

`RawLeafBuilder::required_bytes` (`btree_base.rs:663-681`) is 4 bytes of page header plus 4
bytes per pair for the key length and 4 more for the value length when the type is not fixed
width, plus the payload. Hoard's storage report counts only the payload as `stored_bytes`, so
the "leaf fill" column of `docs/native-catalog.md` understates real occupancy by those length
arrays: two to three points on a 300 byte row.

The page size is 4096 (`src/tree_store/page_store/header.rs:78`) and there is no way to change
it: `Builder::set_page_size` is behind `#[cfg(any(fuzzing, test))]` (`src/db.rs:1176-1181`),
so the method does not exist in a normal build of redb at all. That native_db 0.8.2's own
`Builder` (`database_builder.rs:110-135`) exposes only `set_cache_size` is beside the point;
there is nothing there to expose. It would not help anyway: the midpoint split leaves half a
page whatever the page is.

## 3. Why insertion order does not change it

There are exactly two fast paths in `BtreeMutator::insert_helper`
(`src/tree_store/btree_mutator.rs:218-330`):

- a leaf holding one pair already larger than a page gets the new pair in a page of its own
  (`btree_mutator.rs:231-269`);
- a page this transaction allocated and has not committed is modified in place while
  `LeafMutator::sufficient_insert_inplace_space` says there is room
  (`btree_mutator.rs:274-309`).

**There is no append-biased split.** A B-tree that expects sequential keys usually splits a
full leaf 100/0 when the new key is above every key in it, leaving the finished leaf full;
redb does not, and `build_split` is the only split there is. So a run of ascending keys fills
a leaf in place, splits it in half when it overflows, freezes the left half at about half a
page for good, and carries on in the right half. The steady state is the same 50 per cent a
random order gives.

That corrects a line in `docs/native-catalog.md`: the 42 to 55 per cent is not "ordinary
occupancy after a pass of random-order inserts". A scan's primary keys are close to ascending
already - artifact ids and entry ids come from the allocator in walk order, and the location
index is repository-major - and the fill is 42 to 55 per cent anyway. Order was never the
cause, so ordering is not the cure.

## 4. What a bulk load in key order would do

Nothing measurable. It is the case section 3 describes: in-place growth, then a midpoint
split. One transaction per table would cut commit count and fsyncs, which is a write-time
argument rather than a disk one, and `scan::CHUNK` exists because a transaction holds every
serialised row it writes in memory until commit; a whole table in one transaction at 1M is
gigabytes of resident memory. Both reasons point away from it.

## 5. What a dense rewrite at the end of a fresh scan would cost

The shape would be: finish the scan, open a second catalog file, copy each table into it in
primary key order, fsync, rename over the first. Cost at 1M:

- time: 1.3 GB read and 0.7 to 1.3 GB written through the same encode and decode path the
  scan already pays, plus one fsync per transaction. The 1M scan is minutes; this would add
  a large fraction of one on this workstation.
- temporary disk: a second copy of the catalog, so peak disk is roughly twice the resting
  1,341,190,144 bytes, against a gate of 1,500,000,000.
- risk: a second write path over every record, and a kill in the middle to reason about.

For a saving of, per section 3, zero. The copy would land at the same 50 per cent fill as the
scan did.

## 6. What `Database::compact` does and does not do

`Database::compact` (`src/db.rs:501-566`) commits twice to drain pending free pages, then
loops on `WriteTransaction::compact_pages` (`src/transactions.rs:2065-2125`), which finds the
highest-numbered pages in use, allocates each a lower page with `allocate_lowest`, copies it
and rewrites its parents, until no page can move down. Then the file is truncated.

It moves whole pages. It never rebuilds a leaf, so it returns free pages the file is holding
and returns nothing at all of the room inside pages that hold rows. That is what
`docs/native-catalog.md` measured: the 87.68 MiB residual left by the freed change log was
reclaimable, the 608,835,673 bytes of intra-page slack were not.

## 7. The one case where allocation, not fill, is the waste

`TransactionalMemory::allocate_helper` (`src/tree_store/page_store/page_manager.rs:1052-1069`)
takes `ceil(size / 4096)` pages and rounds that count up to a power of two: a buddy allocator.
A value needing three pages gets four; one needing five gets eight. Rows that fit in a page
are unaffected, and `Cold`'s LZ4 pulls a large row back under a boundary rather than over one,
so this is a reason to keep compressing the two cold tables, not a lever of its own.

## 8. The measurement that would decide it

If someone wants the number rather than the argument, it is half a day:

1. build the 1M fixture with `crates/hoard-index/tests/scale.rs` as
   `.claude/skills/hoard-measure/SKILL.md` describes, and keep its
   `storage-1000000-rebuild.json`;
2. in a scratch binary, open the catalog read-only, create a second catalog beside it, and
   copy `ArtifactDetail` alone into it with a primary scan in key order, chunked at
   `scan::CHUNK`;
3. compare `leaf_pages`, `fragmented_bytes` and `stored_bytes` for that one table between the
   two storage reports.

The prediction this document makes is that `leaf_pages` differs by less than 5 per cent. A
result outside that band means section 3 is wrong about redb and the whole question is worth
reopening.

## 9. Recommendation

**Do not pursue page packing.** It is not reachable from where hoard sits: the fill factor is
decided by one split rule inside redb, insertion order does not touch it, `compact` does not
touch it, and the page size is not a knob any build of redb offers. The two ways to reach it
are both larger than the prize: patch redb, for an append-biased split or for a page size a
release build can set, or leave redb.

Expected saving from what is reachable today: **0 to 5 per cent** of `catalog.redb`, and the
5 is the error bar on the measurement rather than an effect anyone has seen.

The lever that does work is payload. Slack is proportional to the number of leaves, so a byte
off a row is about two bytes off the file, and that ratio held across waves 5, 6 and 7. Keep
taking bytes out of rows and treat 45 per cent slack as the price of redb.
