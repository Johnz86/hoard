# The native catalog

The native_db (redb) plus Tantivy catalog is Hoard's index. `serve`, `scan`, `ls`, the
mirror, the git face and gossip all read and write it, and nothing else persists what Hoard
knows about a file.

This document is the current state. `docs/superpowers/history/2026-09-native-catalog-waves.md`
holds the wave-by-wave record of how it was built.
`.claude/skills/hoard-catalog/SKILL.md` carries the same module map annotated for someone
about to change the code, plus the native_db and Tantivy semantics that the calling code does
not show.

## Decision

`docs/superpowers/specs/2026-09-05-native-catalog-decision.md` records why this pairing was
chosen. native_db was preferred over raw Fjall because it already maintains typed records,
transactions and secondary indexes on redb, so Hoard does not have to own an index
maintenance layer. Tantivy replaced SQLite's FTS5 as a disposable search index that is
rebuilt from the native records alone, without re-reading a single source file.

## Compatibility

Read the "Compatibility" section of `CLAUDE.md`. It is the only statement of the policy;
nothing here restates it.

## Layout

`<data_dir>/native/` holds:

- `catalog.redb`, the one file every record lives in.
- one published `search-v1-<nanos>/` directory holding the Tantivy index. A rebuild writes a
  new directory and publishes its name in a redb transaction only after Tantivy commits; an
  unpublished build leaves the previous one serving, and publishing removes every superseded
  directory.

The whole thing is recreatable from disk plus peers. redb takes an exclusive lock, so one
process may open a catalog at a time; `hoard doctor` reports it when a second one tries.

## How a record is encoded

native_model writes an eight byte header naming the record and its version, then a body in
whichever codec `#[native_model(with = ...)]` names. `catalog/codec.rs` holds two, and the
records that do not name one keep native_model's own bincode default, which is what
`Manifest` bootstraps from before any changed record is read.

- `Compact` is bincode 1.3 with variable length integers, little endian, trailing bytes
  refused. It is on every record but `Manifest` and the three cold ones. native_model's
  default spends eight bytes on every string and vector length, four on every enum
  discriminant and eight on every integer whatever it holds; nothing outside this process
  reads a row, so none of that is paid for.
- `Cold` wraps `Compact`'s bytes in a one byte tag and, when the whole envelope comes out
  smaller, an LZ4 block with a checked four byte decoded length. It is on `ArtifactDetail`,
  `SearchText` and `RevisionText`, the tables nothing scans: the inspector reads one row and
  a rebuild reads one per artifact and one per revision. Each row is compressed on its own,
  so a read is still one primary lookup and one bounded decode, and a length claiming more
  than the 256 MiB maximum is refused before anything is allocated for it.
- `Manifest` alone keeps native_model's default, because `open_checked` reads it to compare
  `format_version` before any record in either of the other two codecs is decoded.

Hashes and row ids serialize as bytes and integers to a binary format and as hex and
prefixed strings to a human-readable one, so the catalog is compact and the JSON the API
answers with is unchanged. `hoard-core`'s `id.rs` is where that branch lives.

`Entry` carries no location text. The unique index over where a row sits is computed from
the three fields that say so, as `repo_id` big endian, then `revision` big endian, then the
relative path's own bytes. The first 8 bytes are exactly one repository and the first 12
exactly one revision, so a repository listing and a revision listing are both one prefix
scan of that index and `Entry` needs no repository index of its own.

`RevisionText` holds one revision's card, keyed by the same 12 bytes an entry's location
starts with, so the card an artifact's documents are indexed from is one point lookup by
`(repo_id, revision)` and one repository's cards are an 8 byte prefix scan. Every artifact of
a revision reads that one row rather than carrying a copy; a build reads it once per run of
entries that share a revision. A card that changes marks every artifact of its revision for
indexing, because nothing else about those artifacts moved.

`ArtifactDetail` keeps at most sixteen of the up to sixty-four tensor samples the sniff draws
for inference, thinned by a stride so the triage queue's hidden dimension is read from a
spread of the file rather than its first quarter; a header already inside the bound keeps all
of it. Inference runs over the whole header before the row is written; nothing prints a sample
by name.

`hoard catalog` takes any directory with `--directory`. Every other command opens or creates
the catalog at `<data_dir>/native`.

## Modules

`crates/hoard-index/src/catalog/`:

- `mod.rs` open, `open_or_scan`, status, integrity, `FORMAT_VERSION`, the per-table storage
  report, compact.
- `codec.rs` the compact and cold record encodings, named by `#[native_model(with = ...)]`.
- `models.rs` every `native_model` record: `Source`, `Repository`, `Revision`,
  `ArtifactRecord`, `ArtifactDetail`, `Entry`, `SearchText`, `RevisionText`, `SearchChange`,
  `Manifest`, `Summary`, `Allocator`, `Node`, `Usage`, `RepoUsage`, and the user-data rows.
- `scan.rs` discovery to sniff to infer to one transaction per unit; `Counters`,
  `DerivedShift`, `log_changes`, the artifact sweep.
- `naming.rs` the config and card a revision carries, artifact names, per-file hashes.
- `search.rs` the Tantivy schema, the rebuild, per-entry documents, the `primary` mark, the
  collectors, `refresh_search` and the published-directory swap.
- `query.rs` the grammar compiled against the Tantivy index.
- `read.rs` the inspector, the repository screens, the triage queue, the sources listing.
- `verdict.rs` a user's override of what an artifact is, re-applied after a rescan.
- `seed.rs` synthetic records for tests, behind `cfg(test)` or the `fixtures` feature.
- `userdata.rs` shelves, settings, the noise list, projections.
- `usage.rs` the served-response log and per-artifact use counts.
- `loadouts.rs` loadout documents and pin resolution.
- `jobs.rs` scan, apply and pull job rows.
- `hashing.rs` sha256 and git blob sha1 per file, content identities, merges.
- `ingest.rs` a file the mirror fetched, filed as a served entry.
- `nodes.rs` this machine and every peer it knows.
- `digest.rs` the rows this node offers and the rows a peer advertises.
- `pulls.rs` a pull in flight and the file it lands.

## What the scan produces

- Existing HF-cache, Git and loose-source discovery feeds the store directly. Header sniffing
  and inference reuse the existing Rust implementations.
- A revision's documents (README, configs, index files) are sniffed first and give every
  later unit its inference context; each shard set and each chunk of 1,024 standalone files
  is then sniffed in parallel, prepared and committed as its own transaction, so memory grows
  with a unit rather than with a source. A rescan settles a revision whose every path carries
  the stamp the catalog stored before it opens any of them, so an unchanged revision costs
  its documents nothing.
- A file is classified by the name its revision gives it, never by the name of the file the
  bytes sit in. An hf-cache snapshot links `README.md` at a blob named by its own hash with
  no extension, so `sniff::sniff_named` reads the header from the blob and takes the format
  from the entry's relative path; a cached revision carries a card and a config the way a
  loose or git one does. A magic number still beats the name, and a loose or git file is
  unaffected because the two names are the same. A rescan cannot find this on its own: an
  hf-cache blob's inode, mtime and size do not change when the rule that reads it does, so
  every stored revision would settle on its stamps. Changing a sniffing or inference rule is
  therefore a `FORMAT_VERSION` bump and nothing else; the bump deletes the catalog and scans
  the disk again, which is the only thing that puts a new rule into an index that exists.
- Only weight and dataset formats become artifacts; every file is still an entry record.
- Shard sets, whether named `model-<n>-of-<m>` or listed by a `*.index.json` weight map,
  become one artifact anchored on the lowest shard. Members are entries pointing at that
  artifact with no search row; the anchor carries the whole model's size and shard count, and
  an incomplete set is triage.
- An unreadable file keeps its path and is recorded as an absent entry with no artifact; a
  git-lfs pointer stub is absent with the pointer's size and hash; a fetched LFS weight is an
  ordinary artifact.
- Every present file is hashed, inline when the scan is asked to hash now and by a background
  job otherwise. An artifact identified by its path becomes identified by its content once
  hashed, merging copies. Artifact identities are 128-bit hashes, of the text `sha256:<sha>`
  or `git:<sha>` when a hash is known and of `path:` followed by the canonical path's own
  bytes when none is, so the unique index holds fixed-width keys and two paths a lossy text
  rendering would fold together stay two artifacts.
- An entry names its revision by an ordinal into the repository's revision list, and the
  ordinal is an identity rather than a position in discovery order. A rescan matches each
  discovered revision to the stored list by commit sha, or by name when it has none: a
  rediscovered revision keeps its slot and takes the fresh refs and metadata, a new one is
  appended, and one the walk no longer finds keeps its slot so the entries filed under it
  still resolve, giving up its refs so a name that has moved answers where it moved to.
- An artifact reached from two revisions is named by the unit that holds more of its
  content, and by the smaller name when they hold the same: a shard set outranks the file
  its anchor is, and two revisions naming one file differently agree whichever of them the
  walk reaches first.
- Ids come from a persisted allocator; the generation increments per rescan; artifact path
  counts are swept from the entry index after every scan and orphaned artifacts are removed.
- A rescan over the whole configuration deletes a stored source the configuration no longer
  names, with its entries, its repositories and the artifacts that lose their last path. A
  path of a dropped source is not a path this node stopped holding, it is a path this catalog
  stopped being about, and a row left behind outlives the source that explains it: nothing
  names its repository's source, the summary still counts it, and re-adding the source files
  every path a second time. A rescan narrowed to named sources retires nothing, and the
  mirror, pull and peer source rows are never a scan's to retire.
- A completion manifest is published after all batches commit, so an interrupted scan stays
  unusable rather than exposing a partial catalog.
- A rescan does not compact, so a catalog shared by the server can be walked again without
  an exclusive borrow. Only a fresh scan compacts, and `hoard catalog scan` compacts again
  after it finishes; `hoard catalog compact` is the on-demand path, and it needs the
  exclusive borrow a running server holds.

## The write path

Every writer of an entry commits through `scan::Counters`:

- `Counters::load` reads the persisted `Allocator` and hands out ids, starting from the
  higher of what the allocator holds and the highest id each table holds, so a pass killed
  before it wrote the allocator back cannot have its ids handed out a second time.
- `Counters::touch(transaction, id)` is called before an artifact record is written and keeps
  what the record contributed to the summary beforehand, so a pass moves the summary by a
  difference rather than recounting every record.
- `Counters::derived` is a `DerivedShift`: `added(&entry)` before writing an entry,
  `removed(&entry)` before removing one or moving it out of presence. It accumulates each
  repository's present files and bytes, each source's owed hashes and each node's filed
  count, and is flushed in the transaction that writes the entries it counted, so a record
  above the entries is rewritten once per transaction rather than once per entry and a
  killed pass leaves no count behind the rows that moved it.
- `log_changes(transaction, counters)` writes one `SearchChange` row per touched artifact, in
  the transaction that made the change, and writes nothing while no search directory is
  published. The sequence continues past both the highest key the log holds and
  `Manifest.search_seq`.
- `Counters::save(transaction)` flushes whatever shift is left and writes the allocator back.

A rescan recognises a stored row by where it is filed. `known_stamps` keys the map it hands
the walk by a hash of the location - the repository id, the revision ordinal and the relative
path - and not by the absolute path the row's bytes were last found at. The two are not the
same identity: an hf-cache entry sits at a blob under `blobs/`, so a file rewritten in place
is discovered at a new absolute path, its stored row would never be recognised, and the row
written for it would collide on the unique `location` key. The location is what the catalog
itself files an entry under, so the walk and the index agree on what one row is, and it costs
one hash of a relative path where the absolute path cost one of a longer string.

A location is an ordinal, so a revision the walk finds at a fresh slot matches nothing under
that key though every file it holds is a file the catalog already read. The walk therefore
builds a second index, from the repository and the relative path to the one location that
holds it, and `refiled_of` consults it only for a revision the catalog holds no rows for at
all: a stamp-equal hit is refiled onto the new ordinal through `Walk::moved` rather than read
again. A path more than one stored revision holds is left out of that index, because which of
them moved is not knowable from the path and taking the wrong one would empty the revision it
is filed under; such a path is prepared and stored as a row of its own. So is a path whose
row sits on a revision this walk discovered: that revision is still on this disk, so nothing
moved, which is what two hf-cache snapshots linking one blob look like.

That index is built one repository at a time, as the walk reaches a revision the catalog
holds no rows for, and not for every source before the walk starts: only a repository with a
fresh slot pays for it and only one repository's worth is held. The map of stored rows
itself is unavoidable and is 48 bytes a row.

A pass that commits many times can also be asked to stop. `Catalog::rescan_until` checks a
`Stop` before each source it discovers and between the units of the one it is inside, and
`Catalog::hash_pending_until` checks it before the pass opens, before each batch and once per
megabyte of every file it reads, so a shutdown costs a megabyte rather than an 80 GB
checkpoint. What it cannot cut short is the discovery walk of the source it is already
inside, so an exit costs a batch, a unit or one source's discovery, never a whole pass. Both
passes leave the pass mark set and retire nothing: an interrupted walk never looked for the
paths it did not reach, so it must not retract them, and the next pass settles the counts and
finishes the hashing. A hashing pass is not started behind an interrupted walk at all, since
opening a pass whose mark is held counts every artifact record before its first batch would
break on the same stop. The scan lane raises that stop when it closes, finishes the job that
was running with its phase set to `interrupted by shutdown; resumes on the next scan`, and
cancels the requests that had not started.

`Known`, the row that map holds, is 48 bytes: an entry id, an artifact id, a stamp, an
ordinal and a shard count, with the two absent cases spelled as zero rather than as an
`Option` discriminant, because ids come from the allocator and are positive and a set of no
shards is not a set.

**Every writer of an entry moves the derived counts.** A writer that skips `DerivedShift`
leaves `Repository::present_files`, `Repository::present_bytes`, `Source::pending_hashes` and
`Node::filed` behind; `hoard doctor`'s summary check is what catches it. A writer that skips
`log_changes` leaves the search index stale with no record that it is.

A pass that runs before any index is published writes no `SearchChange` rows at all. There is
nothing for them to be a difference against: the build that follows reads every artifact
record, and publishing prunes the whole log unread. `Counters::load` reads whether the
manifest names a published directory and `log_changes` returns early while it does not, so a
fresh scan of a million artifacts no longer writes and then deletes a million rows.

The usage log carries a second discipline of the same shape. `log_usage` and
`log_usage_batch` write each served response and, in the transaction that wrote it, move the
`RepoUsage` record of every repository the write served: its pull count, its sorted client
list, its last pull and how many pulls each of the last 31 UTC days holds. The USAGE tab's
client count and its chart are both read from that record rather than counted from the log,
so drawing the chart is one point read whatever the log holds; a window longer than the
record keeps is counted from the log instead. A `Usage` row carries a `repo_at` secondary key
- the repository id in big-endian bytes followed by the row's RFC 3339 time - so the recent
pulls are the tail of one range read backwards. A writer of a usage row that skips the record
leaves the client count and the chart behind.

The triage threshold a scan ran with is stored in the manifest, and every writer that counts
an artifact into the summary counts against the stored value rather than against its own
configuration: `hoard doctor` on a catalog scanned at 0.5 recounts at 0.5. A rescan with a
different configured value records it and counts the summary again; each artifact's own
stored state is restated as that walk reaches it, not before.

`refresh_search` replays the log after the durable cursor, one Tantivy commit per batch,
deleting an artifact's documents by term and re-adding its entries, then prunes what it
applied. A rescan followed by a refresh updates search without a rebuild. A query answers
from the published index even when a rescan has logged changes no refresh has applied, and
says so with `stale`; only a catalog with no index at all is refused. A candidate on the page
whose entry, or the artifact that entry named, the rescan replaced is skipped and counted,
and that count sets `stale` as well, so a query landing in the window between a rescan and
its refresh answers with the rows that are still there. A repository an entry still names but
which is gone stays an error: that is corruption, not staleness.

`rebuild_search` and `refresh_search` are the two writers of the search directory and of the
manifest's search fields, and they hold `Catalog::search_lane` for their whole run so they
never overlap. It is a blocking `parking_lot::Mutex`, which does not poison, so a panic in
one rebuild does not stop every later refresh; an async caller reaches it through
`spawn_blocking` like every other catalog call, and nothing async holds it. Row writers need
no lane of their own: redb allows one writer at a time and serialises commits.

A rebuild reads the highest logged sequence **before** its snapshot opens and publishes with
that number, so `publish` prunes only what the build actually indexed. A change logged while
the build ran stays above the cursor and the next refresh applies it. Publishing with the
highest sequence at publish time instead would drop that change unapplied with no record
that it happened.

### Recovery

A pass is a sequence of commits and every one of them has to leave a catalog the next run
completes. What each stage leaves:

- **The walk** commits one transaction per unit, and every count derived from the entries in
  it moves in that same transaction. A killed walk leaves the units it committed; the next
  rescan skips them, because the stamps stored for them are the stamps the files still have,
  and prepares the rest.
- **A fresh scan** writes no manifest until it finishes, so a directory holding a catalog
  file and no manifest is not a catalog. `Catalog::open` refuses it as `CatalogIncomplete`
  and `open_or_scan` removes the directory and scans a fresh one, which is what it already
  did for a catalog of another format.
- **The totals**, and what each scanned source holds, are what a pass settles at its end
  rather than in the transactions that earn it. A pass that commits more than once sets
  `Manifest.pass_open` before its first commit and clears it in its last, and a run that finds
  the mark set counts every artifact record again, and each source's files and bytes again
  from its repositories, before it starts. Only a kill pays for that; `Catalog::open` pays it
  once for the process that died.
- **An artifact the pass takes a path from** is recounted in the transaction that took it, so
  a record counting a path no entry backs is never committed. The sweep at the end of the pass
  repeats it and finds nothing, and a kill in between leaves nothing to find: the next walk
  skips the rows the killed pass wrote, so it would never look at that artifact again.
- **The hash pass** commits every 64 digests with the derived counts in the same transaction,
  so the next pass reads the files still owing a hash and no others.
- **The refresh** commits Tantivy, advances the cursor and then prunes what it applied. A
  kill before the cursor replays the batch, and a replay deletes an artifact's documents by
  term and adds them again, so it changes nothing. A kill between the cursor and the prune
  leaves rows the cursor has passed, which are what `pending_changes` counts: a refresh drops
  them before it reads, because otherwise the catalog reports a staleness no refresh clears.
  Both of those points are inside one batch and the callback fires after all three steps, so
  the test stages them by hand - it rewinds the cursor and puts the pruned rows back, and it
  writes a row at the cursor - rather than adding a hook to reach them.
- **A rebuild** publishes nothing until Tantivy has committed, so a killed one leaves the
  previous index serving and a `search-v1-*` directory the manifest does not name. Publishing
  was the only thing that removed a superseded directory, so `Catalog::open` now removes every
  one the manifest does not name.

What this does not cover, and is accepted: a redb file torn by a power loss is redb's own
recovery; and `retire_entries` and `retract_digest` settle the totals at the end of their own
multi-transaction passes without the mark, so a kill inside one of those leaves the totals to
`hoard doctor`.

The `recovery` module of `catalog/tests.rs` holds the proof. It interrupts a stage from the
progress callback the stage already takes - counting the calls, panicking on a chosen one and
catching the unwind - and compares what the next run completes to against a clean run of the
same fixture, through one fingerprint of the totals, the counts, every source, repository,
node, artifact and entry, and the first page of the default query. On the same call it copies
the catalog directory, which is what a power loss leaves for a stage whose writer is the
calling thread: every redb commit fsyncs and the copy is taken between two of them.

## Search document shape

The index contains **one document per entry**. This preserves a requirement the earlier
one-document-per-artifact proposal missed: `node:laptop AND state:local` must match one entry
satisfying both predicates, and the preferred row must be chosen from the entries that
matched rather than fixed before filtering.

One document per artifact carries a `primary` mark: the entry a row would be assembled from,
which is the highest availability preference and on a tie the lowest entry id. A query that
filters and sorts only on fields every entry of an artifact agrees on is intersected with
that mark and needs no collapse.

A query that does collapse walks every matching entry document instead, and folds them as it
goes. The documents of one artifact are added as one writer group, which tantivy flushes into
one segment, and a merge stacks each segment's live documents in order, so inside a segment
an artifact's documents are a run of consecutive doc ids. `Collapsing` holds the best entry
of the open run and emits one row when the artifact id changes, and once more when the
segment ends, so every collector downstream sees one row per artifact and none of them holds
a row per matched entry. `QueryPath` names which of the two a collector walks and the
fold is the other arm of every one of them, so both paths select, sample, bucket, tally and
page through the same code, and both count every artifact once and tally it into every facet
dimension once.

The window is decided by the pass that counts it; nothing counts the match in a pass of its
own.

For A matched artifacts and K equal to offset plus limit, a window with K at most 16,384 for
a term key, or 131,072 for a numeric one, is **one pass**: it keeps a heap of the K rows at
the front of the order and reports the total, which is what the window is then cut by. That
is O(M) time over M matched documents and O(K) memory per segment, plus one tally slot per
term of each facet column's dictionary in each segment, and page selection O(K log K). Both
constants are measured, in "Thresholds" below; they differ because the heap of a term key
spells a term for every row it holds and a numeric key only compares integers.

A deeper window starts with the sampling pass, which counts and tallies the match and keeps
every S-th row's sort key. With the total in hand the window is placed: one nearer either end
than the shallow bound is answered by a second pass keeping a heap of the reversed order, and
one in the middle by two more, the first counting the rows between consecutive sampled keys
and the second keeping the rows of the one bucket that holds the requested rank plus a heap
of one page beyond it. That is two or three passes, O(M log S) time and O(S + B + limit)
memory for a bucket B capped at 65,536.

The stride S is about the square root of the number of documents the index holds, floored at
64 boundaries and capped at 4,096. It comes from the index rather than from the match because
nothing has counted the match when the sampling pass starts, and it bounds what matters
either way: a bucket holds about one stride of rows however selective the query is, and the
boundaries a document is placed against are the match divided by the stride, so never more
than the square root of the index. The square root is where the two costs meet: mapping a
boundary into a segment costs a dictionary seek, so the counting pass starts at
O(S x segments). The stride is forced odd, because a stride sharing a factor with a period in
the order documents were written samples only some residues of that period and leaves whole
runs of equal keys with no boundary between them.

A bucket over the cap costs more to hold than the heap does, so the heap answers the request
instead and nothing about the answer depends on the sample. Either way, a window nearer the
far end is cut from the reversed order, so the deepest window a rank selection sees is half
the match.

A query naming `node`, `state`, `source`, `repo`, `rev` or `hash`, or sorting on revision,
node or state, still collapses, because those fields differ between the entries of one
artifact. It runs the same selections over folded rows, so its memory is the page and the
sample exactly as the primary path's is; the fold costs one open row per segment.

Keyword columns are string fast fields and the collectors keep **term ordinals**, not terms.
Ordinals are lexicographic inside a segment and meaningless across segments, so a page
ordered by a term sorts on ordinals while one segment holds every hit and otherwise bounds
each segment on its own ordinals and merges the survivors by the terms they spell. A sampled
boundary crosses segments the same way: its ordinal is spelled once, the terms settle the
order between boundaries, and each boundary is then mapped back into every segment as the
lowest ordinal whose term is not below it, with whether that ordinal spells the boundary's
own term rather than the next one above it. A term is spelled once per segment and ordinal,
only for a page row, a boundary, a facet bucket or the node set. A page nearer the end of the
order is cut from the reversed order, so a deep offset selects and spells no more than a
shallow one.

A tally is a **vector indexed by term ordinal**, one per facet dimension per segment, sized
at that column's `num_terms()`; the base dimension carries a second vector of the label
ordinal its artifacts agreed on. A segment ordinal is a constant inside a segment collector
and a facet dictionary is small, so counting a document is an increment at an index rather
than a hash of a `(segment, dimension, ordinal)` key. The node total reads the ordinals of
the node dimension that at least one document sat on. Segments are merged by the terms their
ordinals spell, once per segment and value.

Documents are read **a block at a time**. A collector that takes no score is driven through
`for_each_no_score`, which hands `collect_block` up to 64 documents at once, so each column
is asked once per block through `Column::first_vals` into a buffer the segment collector
reuses, rather than once per document per column. `collect` is one block of one document, so
there is one path. All four collectors read this way: the two that tally read the facet
ordinals and the base label as well as the row on the primary path, and on the collapsed one
read the row alone and take the chosen entry's ordinals out of the columns once the run
closes; the window collector reads the row alone, and the bucket collector reads only the
sort key and the artifact id where it does not have to fold.

Every collector overrides `Collector::collect_segment`, because tantivy's own
`default_collect_segment_impl` drops to one `collect` per document as soon as a segment has a
delete file, and every segment a refresh has touched has one. The alive filter therefore lives
here instead, filtering each block into a buffer the segment reuses, so a block read survives
deletes. Without it the block path would be dead on exactly the segments a long-lived index
accumulates, and slower than what it replaced.

`Catalog::query` answers a whole index screen: the page in any of the ten sort orders, the
distinct artifact and node totals, and the five facet dimensions. Each dimension is counted
over the query with its own key excluded, so clicking a bucket never collapses the rail it
was clicked in. The `base` rail is at most 32 buckets, each counted by running its own query,
because the prefix rule that groups base labels cannot be answered from the tally. The
candidates are the most populous bases by exact term plus every family root among the rest
that one of them extends, since a root carried by few artifacts but prefixing many is a
larger bucket than its tally says; the whole candidate set is counted and then cut to 32 by
that count. Only a base the tally already holds is a candidate, so the cap drops buckets and
never invents one, and the candidate set is bounded, so the searches one query pays for the
rail do not grow with the number of bases the catalog holds. A collapsed query counts a
bucket by folding the artifact id alone, with no tallies and no row.

Free text indexes names, organizations, bases, triggers, tensor key prefixes and the body of
the shallowest README in each revision. Tantivy uses one writer thread with a 32 MiB indexing
budget; nothing larger than a unit is held in memory or in an open transaction.

## Running it

Every command opens or creates the catalog at `<data_dir>/native`:

```sh
cargo run -- --config hoard.toml scan
cargo run -- --config hoard.toml serve
cargo run -- --config hoard.toml ls 'role:lora size:<400mb'
```

The `hoard catalog` commands maintain a catalog directly, on any directory. The parent of the
output directory must already exist. A new directory is scanned fresh; an existing catalog
directory is rescanned in place: unchanged files are not opened, changed files are read
again, missing files keep their row as absent, and the search index is refreshed from the
change log afterwards, or built when there is none yet.

```sh
cargo run -- catalog scan --directory /existing/path/native-catalog
cargo run -- --config hoard.toml catalog scan --source cache --directory /existing/path/native-cache
cargo run -- catalog search --directory /existing/path/native-catalog 'format:gguf'
cargo run -- catalog search --directory /existing/path/native-catalog --sort size --desc 'base:sdxl'
cargo run -- catalog status --directory /existing/path/native-catalog
cargo run -- catalog rebuild --directory /existing/path/native-catalog
cargo run -- catalog compact --directory /existing/path/native-catalog
```

Run `search`, `status`, `rebuild` and `compact` one after another; the lock is exclusive. A
scan that fails removes the directory it started, and a source whose root is missing is
skipped with a warning.

`serve` also writes the catalog without being asked to: it watches every configured source
unless `[index] watch = false`, and each debounced burst of filesystem events is one scan
request over the sources it named, under the configured `hash_policy`. A kernel queue
overflow, a signal the watcher could not queue, and every `reconcile_interval_secs` (3600 by
default, 0 to turn it off, and once at startup) each ask for a scan of every source instead,
which is cheap on an unchanged tree because a rescan short-circuits on stamps. Events under
the server's data directory, the index's data directory or the mirror cache root are dropped
before the debounce, so the catalog, the block cache and a projection's symlinks never cause
a rescan of a source they sit under. Every one of those roots and every source root is
resolved to the absolute path an event carries first, because a root spelled relative to the
working directory would match nothing. Every one of these is a request on the same lane
`POST /api/v1/scan` uses, so a watched scan and a manual one never run at once.

`compact` is the only thing that compacts a catalog on demand. A scan compacts at the end of
building one, because that is bulk construction; a rescan never does, so a shared catalog
stays readable. `compact` reports the bytes `catalog.redb` was allocated before and after,
and what that difference reclaimed. It needs the catalog to itself: a catalog another process
holds is refused rather than waited for.

## The query grammar

Search speaks Hoard's own grammar, the one the search bar, `hoard ls` and shelves use. Tokens
combine with AND, a token's comma-separated values with OR, and a leading `-` negates the
whole token. Every token kind compiles:

- `role:` matches the role, and narrows to a sub-role when the spelling names one, so
  `role:ip-adapter` is `lora` with sub-role `ipa`.
- `base:` (also `arch:`) matches a family or a family it prefixes either way, and never
  matches `any`, which is a claim about compatibility rather than about a family.
- `format:` (`fmt:`), `precision:` (`prec:`, `quant:`), `state:` and `source:` (`src:`) are
  equality on that column.
- `node:` (`host:`) matches the node, resolving `self`, `me`, `local` and `here` to this
  catalog's own node.
- `rev:` (`revision:`, `tag:`, `branch:`) matches the revision name or a prefix of its sha.
- `repo:` matches the qualified name or any suffix of it, so `repo:name` finds
  `models/org/name`.
- `org:` (`owner:`) matches the artifact's org or the repository's org.
- `hash:` (`sha:`, `sha256:`) matches a sha256 exactly or by prefix, and a git blob sha
  exactly.
- `trigger:` (`triggers:`) matches the whole trigger string or one of its words.
- `dupe:` (`duplicate:`) selects artifacts reached by more than one path.
- `size:` compares a byte count with `<`, `<=`, `>`, `>=` or none for equality; a bare number
  is megabytes, so byte counts need the `b` suffix.
- `used:` compares an age in days against when an artifact was last served: `<Nd` and `<=Nd`
  are the artifacts used inside the window, `>Nd`, `>=Nd` and `=Nd` the ones nobody has used
  for that long. An artifact that was never served answers neither.
- `compat:` resolves its value against this catalog and then matches the roles and bases that
  plug into it; a value naming nothing is not an error, it resolves to no target and the
  token is refused.
- `shelf:` expands the named shelf's stored query, up to four levels deep.
- Anything else is free text: every word has to appear, and `~` in front also accepts the
  words as prefixes. A key the grammar does not know is free text carrying a warning.

Free text and `trigger:` match whole words or the whole stored string rather than substrings.

`--sort` takes `role`, `name`, `base`, `format`, `precision`, `size`, `revision`, `node`,
`state` or `used`, defaults to `name`, and `--desc` reverses it; every order ends on ascending
artifact id so paging is stable. `--limit` is capped at 1,000 and `--offset` selects the page.

## Measured

Read `.claude/skills/hoard-measure/SKILL.md` before adding a row here. Day-to-day timings on
this workstation drift by more than most effects being measured, so a "before" number quoted
from this document is not a comparison: rebuild the previous commit and measure both in one
session.

### This workstation's disks, commit `7bb681f`

Release build, the three sources of `hoard.toml` copied into a scratch data directory under
the session scratchpad: the HuggingFace cache, the ComfyUI tree and the LM Studio tree.

```sh
hoard --config <scratch>/workstation.toml catalog scan \
  --directory <scratch>/workstation/native --hash never
hoard --config <scratch>/workstation.toml doctor
hoard --config <scratch>/workstation.toml doctor --repair
hoard --config <scratch>/workstation.toml serve
hoard --config <scratch>/workstation.toml ls 'role:lora'
```

- Scan: 3.465 s wall, `unchanged=728 changed=0 added=0 absent=0`, 3 sources, 103
  repositories, 230 artifacts, 728 entries, `format_version: 6`. The reading at `1ba8475`
  was 5 m 35.686 s under `hash_policy = "background"`, nearly all of it the 445 files read
  end to end for a content hash; with `--hash never` no file is read past its header, so the
  two numbers measure different work and are not comparable.
- `doctor`: every check passes. git 2.43.0, the data directory, the catalog at format 6
  generation 1, the node `workstation`, zero peers, the summary at 230 artifacts, 230
  present and 103 repositories, the search built with 0 pending changes, the three sources
  at 477 files and 414.4 GB (`hf-cache`), 223 files and 819.7 GB (`comfy`) and 28 files and
  405.8 GB (`lmstudio`), the block cache not created yet and the upstream. Exit status 0.
- `doctor --repair`: `no drift; the stored totals were already exact: Totals { artifacts:
  230, present_artifacts: 230, triage_artifacts: 24, indexed_bytes: 1559570182409,
  repositories: 103, advertised_artifacts: 27 }`. Every other check passes as above.
- `serve` for one minute with the watcher on: 31 ms from exec to the first answered
  `/healthz`, `watching=3 unwatchable=0 missing=0`, 439 inotify watch descriptors held by
  the process, `{"status":"ok","index":"ok","upstream":"ok","watch":"ok","peers":0}` at
  startup and at 60 s, and 15 ms from SIGTERM to exit.
- The first `serve` after a scan that skipped hashing is not that. The watcher reconciles
  once at startup, that scan runs the hashing pass the scan was told to skip, and an orderly
  shutdown joins the lane, so SIGTERM was answered in about 4.5 minutes rather than 15 ms.
  Nothing is lost and the second run costs 15 ms because the hashes are stored, but a
  shutdown waiting on a hash pass of 445 files is a wait a caller cannot see the end of.
- After that hashing pass the summary is 228 artifacts, not 230: content hashes merged two
  artifacts the header sniff had filed apart, which is the `merged=2` the `1ba8475` reading
  also saw.
- Catalog on disk: 5,496,832 bytes allocated over the directory, of which `catalog.redb` is
  4,198,400 and the published search directory 1,298,432. At `1ba8475` the same three
  sources came to 6.9 MB with a 6.3 MB `catalog.redb` and a 572 KB search directory: the
  record file is 2.1 MB smaller and the search directory 726 KB larger, because every
  cached revision now has a card. The `RevisionText` table holds 16 rows and 127,658 stored
  bytes, which is those READMEs and configs; before `7bb681f` no hf-cache revision had a
  card at all.
- `ls 'role:lora'`: 35 artifacts in 17 ms, which is process start and opening two files
  rather than the query.

### Routes at 100k entries, commits `3a9fa1f` and `7bb681f`

A synthetic tree of 100 loose repositories of 1,000 valid safetensors headers each, plus a
git clone of 1,000 more committed on `main` as `org0/repo000`, so `resolve` names a
repository a commit names and the model route lists a repository of the same size as the
loose ones: 101 repositories and 101,000 entries. `hash_policy = "never"`, `offline = true`,
the watcher off, and 2,000 usage events seeded on `org0/repo000` before the first timed
request. Both columns were scanned, served and timed in one session over the same tree, one
server at a time under `timeout`. Six runs per route across two server starts, median, whole
request including the response body.

| route | `3a9fa1f` | `7bb681f` | before min-max | after min-max |
| --- | --- | --- | --- | --- |
| `GET /api/v1/repos?limit=1` | 0.46 ms | 0.76 ms | 0.40-0.70 | 0.53-0.81 |
| `GET /api/v1/repos?limit=100` | 0.63 ms | 0.78 ms | 0.54-0.84 | 0.66-0.94 |
| `GET /api/models?limit=1` | 0.54 ms | 0.54 ms | 0.38-0.60 | 0.44-0.82 |
| `GET /api/models/org0/repo000` | 2.79 ms | 4.55 ms | 2.73-3.42 | 4.23-5.38 |
| `HEAD /org0/repo000/resolve/main/w0.safetensors` | 0.65 ms | 0.72 ms | 0.39-0.77 | 0.60-1.14 |
| `GET /api/v1/sources` | 0.37 ms | 0.45 ms | 0.28-0.70 | 0.23-0.65 |
| `GET /api/v1/search?q=` | 3.71 ms | 5.58 ms | 3.47-4.85 | 5.41-8.05 |
| `GET /api/v1/search?q=node:self` | 22.20 ms | 11.70 ms | 18.70-24.63 | 11.41-11.82 |
| `GET /api/v1/search?q=role:lora` | 3.99 ms | 5.14 ms | 3.94-4.24 | 3.87-6.47 |
| `GET /api/v1/triage?limit=20` | 4.64 ms | 3.45 ms | 3.49-6.11 | 3.43-3.60 |
| `GET /api/v1/repos/models/org0/repo000/usage` | 1.50 ms | 2.32 ms | 1.38-1.55 | 2.23-2.86 |

Two routes are the work of these waves. `node:self` is the collapse path bounded in wave 4:
22.20 ms to 11.70 ms, and its spread does not reach the difference. `triage` is the
`triage_queue` index of wave 5b: 4.64 ms to 3.45 ms, and it is the only route whose spread
narrowed to nothing.

Three routes are slower by more than the spread of their own runs.

- `GET /api/v1/repos/models/org0/repo000/usage`, 1.50 to 2.32 ms, is the USAGE tab read of
  wave 5b and the reason is in the code: one read of the repository's whole log became a
  scan of the seven-day window by the `repo_at` key, a second scan for the newest pulls, and
  a point read of the repository's own `RepoUsage` row for the client count. On this fixture
  every one of the 2,000 pulls is inside the window, so the new path reads what the old one
  read and pays for two scans and a lookup instead of one scan. What it buys is the
  asymptote: the old route was O(rows of the repository) for a count it can now read.
- `GET /api/models/org0/repo000`, 2.79 to 4.55 ms, lists 1,000 siblings. The handler is
  byte-identical between the columns and the response body is byte-identical at 33,288 bytes.
  **Not a regression:** wave 9 re-measured it paired and warm and the difference between the
  two columns is +0.12 ms in one dataset and -1.19 ms in the other. See "The fix wave's
  rows".
- `GET /api/v1/search?q=`, 3.71 to 5.58 ms, is the default page of 100 rows. The response is
  byte-identical at 72,082 bytes and the same query through the harness at 100k costs the
  same in both columns (2.72 against 2.65 ms). **Not a regression either:** paired and warm
  it reads +0.01 and -1.99 ms across two datasets, inside the round-to-round swing of the
  control-normalised numbers. Both rows above were taken as three unwarmed runs per start
  over two server starts, in a session of their own per column; that protocol carries about
  a millisecond of drift, which is more than the difference it reported.

The reading at `64e11af` is history from a tree built the same way but before the tally
wave, and its query rows were 13.7, 41.6, 16.1 and 9.35 ms where this tree reads 3.7, 11.7,
5.1 and 3.4; only the two slower rows above are within reach of it, at +1.17 ms for the
model route and +0.93 ms for the usage route.

### The fix wave's rows, commits `3a9fa1f`, `7a57762` and `2c866f5`

Wave 9 re-measures the rows wave 8 could not accept, and only those: the deep offset at 1M,
the three routes at 100k, the five 1M rows that were 1.5 to 3.6 per cent slower, and the
workstation shutdown. Every column below was built, scanned, served and timed in this one
session on this one machine over one tree, release, foreground, one process at a time, no
`RUSTFLAGS`.

**At 1M.** One run of `crates/hoard-index/tests/scale.rs` per column over the same tree, in
the order `3a9fa1f`, `7a57762`, head, then the head again after its last commit. The two head
runs bound the run-to-run spread at about 1 per cent.

| row | `3a9fa1f` | `7a57762` | head, first run | head, final |
| --- | --- | --- | --- | --- |
| query empty by name | 21.082 ms | 21.333 ms | 21.230 ms | 21.046 ms |
| query deep offset | 21.405 ms | 38.566 ms | 21.604 ms | 21.453 ms |
| query midpoint offset | 71.842 ms | 73.008 ms | 72.923 ms | 71.800 ms |
| query midpoint offset by size | 58.097 ms | 59.389 ms | 59.266 ms | 58.600 ms |
| query role:lora with facets | 23.969 ms | 24.574 ms | 24.024 ms | 23.763 ms |
| query filtered | 38.023 ms | 39.365 ms | 38.371 ms | 37.993 ms |
| query node:self with facets | 152.699 ms | 50.605 ms | 50.650 ms | 50.149 ms |
| query node:self deep offset | not measured | 74.674 ms | 53.712 ms | 53.107 ms |
| peak rss growth during rescan and refresh | 58,093,568 | 286,826,496 | 60,870,656 | 58,257,408 |
| scan wall | 75.657 s | 74.689 s | 76.349 s | 74.786 s |
| rebuild wall | 7.676 s | 8.620 s | 7.189 s | 8.463 s |
| allocated disk after the rebuild | 1,398,951,936 | 1,396,858,880 | 1,396,785,152 | 1,397,518,336 |

The deep offset is the row wave 2 of this document's thresholds table missed: 38.566 ms at
`7a57762` against 21.405 ms at `3a9fa1f` measured beside it, and 21.453 ms at the head. The
count pass in front of a deep window restores it, and it takes the collapse path's far end
with it, 74.674 to 53.107 ms. The five rows that were 1.5 to 3.6 per cent slower are inside
1 per cent of the `3a9fa1f` column in the final run and inside the spread of the two head
runs; no collector was monomorphised to get there. The rescan and refresh hold 226 MB less
than `7a57762` did, which is the per-repository path index and the 48-byte `Known` row.

**Routes at 100k.** The same tree, catalog and seeded usage log for each column, servers
started one at a time under `timeout`, five warm-up rounds discarded, then ten timed rounds
of each route per start, six starts per column, the columns interleaved so a drift in the
machine hits all three. Two independent datasets of six rounds each were taken, the second
after 600 GB had passed through the page cache.

| route | `3a9fa1f` | `7a57762` | head | paired `7a57762` - `3a9fa1f` | paired head - `3a9fa1f` |
| --- | --- | --- | --- | --- | --- |
| `GET /api/models/org0/repo000` | 2.715 / 3.950 ms | 2.815 / 2.721 ms | 1.699 / 1.766 ms | +0.12 / -1.19 ms | -0.99 / -1.62 ms |
| `GET /api/v1/search?q=` | 3.458 / 5.397 ms | 3.508 / 3.436 ms | 3.373 / 3.390 ms | +0.01 / -1.99 ms | -0.10 / -0.15 ms |
| `GET /api/v1/repos/.../usage` | 1.523 / 2.047 ms | 1.733 / 1.672 ms | 1.039 / 1.042 ms | +0.19 / -0.41 ms | -0.45 / -0.82 ms |
| `GET /healthz`, the control | 0.376 / 0.416 ms | 0.434 / 0.409 ms | 0.400 / 0.387 ms | +0.05 / +0.02 ms | 0.00 / -0.03 ms |

The paired columns are the median over six rounds of the per-round difference, each round
being one server start of each column in turn. **The two routes wave 8 could not attribute
did not slow.** Between `3a9fa1f` and `7a57762` the model route reads +0.12 ms in one dataset
and -1.19 ms in the other, and the default search page +0.01 and -1.99 ms; the per-round
differences run from -2.2 to +2.1 ms in both directions while the `/healthz` control stays
inside 0.05 ms. Wave 8's +1.76 and +1.87 ms were two unpaired sessions of three unwarmed runs
each, which is a protocol whose own noise is larger than the effect it reported. Nothing in
the delta had to be found, and no bisection was run: the hypothesis the closing review asked
to test first, that the gap is page layout, needs a gap.

Both routes are nevertheless faster at the head than at either earlier column, in every round
of both datasets, and each has a mechanism: the model route no longer reads one artifact
record per file to resolve a `deduped` mark the Hub routes throw away, about 1,000 random
point reads per request; the usage route reads its chart off the repository's own record
rather than decoding every row of the seven-day window.

**The workstation shutdown.** The three configured sources of this workstation, 728 entries
over 1.6 TB, scanned with `--hash never` into the scratchpad, then served under
`hash_policy = "background"` with the watcher on, so the startup reconciliation runs the
hashing pass the scan skipped. Measured at the head, with `discovery = "static"` and
`offline = true` so nothing but the disk is in the number.

| measure | at `7bb681f` | at the head |
| --- | --- | --- |
| SIGTERM to exit with a hashing pass in flight | about 4.5 minutes | 0.040 s |
| SIGTERM to exit with nothing left to hash | 15 ms | 0.019 s |
| what the interrupted pass left | not measured | 213 of 237 files still owing a hash |
| the job row it settled with | not measured | done, phase `interrupted by shutdown; resumes on the next scan` |
| the next start | not measured | hashed the remaining 213 files in 336 s, then `hoard doctor` reads `ok` on every check |

### The scale harness

`crates/hoard-index/tests/scale.rs`, an ignored test driven by `HOARD_SCALE_ENTRIES` and
`HOARD_SCALE_DIR`. Synthetic loose source of header-only safetensors files, 1,000 per
directory, one artifact per file, no hashing, release build, its own process. The catalog
produced exactly one artifact per file at 1,000, at 100k and at 1M. The harness then rewrites
1,000 of the headers, rescans and refreshes the search index from the change log, so the
incremental path is measured beside the full one. It puts those 1,000 headers back before it
returns, so the next binary measured against the same tree reads exactly the same input.

The harness closes the catalog three times, because redb's per-table statistics need the
exclusive lock: after the scan, after the queries, which is the state the rebuild left, and
after the incremental workload. Each close writes a JSON report beside the catalog and
prints it. Reopening drops native_db's 256 MiB redb cache, so the rebuild and the rescan run
against a cold record cache and a warm operating-system page cache; that is the same for
every column taken with this harness and is not comparable to the columns above it.

It then rebuilds the search index a second time, over the published one, which is the only
way to see what a rebuild costs in temporary disk: the fresh build has no superseded
directory beside it.

`supplemental_catalog_costs` in the same file is a second ignored test, behind the
`fixtures` feature, for the costs the one-artifact-per-file gate cannot show. It seeds
records rather than sniffing files: long relative paths, an hf-cache source whose entries
carry an explicit blob path, populated sha256 and git blob shas, a full model card and
tensor summary per artifact, three revisions per repository, and one artifact in ten reached
by more than one path.

```sh
HOARD_SCALE_DIR=<scratch> cargo test --release -p hoard-index --features fixtures \
  --test scale -- --nocapture --ignored supplemental
```

At 1M. `95cd3ad` is the base this wave started from and `1783f2b` is the wave as it landed;
both were measured in one session on this workstation with the harness at `ef29745`, on an
otherwise idle machine. Exact bytes, with MiB beside them.

| measure | 95cd3ad | 1783f2b |
| --- | --- | --- |
| scan wall | 98.808 s | 76.395 s |
| peak rss growth during scan | 879,169,536 (838.44 MiB) | 880,521,216 (839.73 MiB) |
| rebuild wall | 13.634 s | 7.512 s |
| peak rss growth during rebuild | 343,195,648 (327.30 MiB) | 351,121,408 (334.86 MiB) |
| on disk after rebuild, allocated | 2,387,607,552 (2277.00 MiB) | 1,399,648,256 (1334.81 MiB) |
| of which `catalog.redb` | 2,304,364,544 | 1,341,190,144 |
| of which the search directory | 83,243,008 | 58,458,112 |
| record payload after rebuild | 1,042,201,188 (993.92 MiB) | 563,971,405 (537.85 MiB) |
| on disk after the churn, allocated | 2,387,750,912 | 1,403,404,288 |
| query empty by name | 117.374 ms | 119.359 ms |
| query deep offset | 118.702 ms | 118.851 ms |
| query midpoint offset | 1.626 s | 1.529 s |
| query role:lora with facets | 141.044 ms | 143.290 ms |
| query filtered | 225.676 ms | 229.826 ms |
| query node:self with facets | 474.983 ms | 408.324 ms |
| inspector artifact_detail | 25.296 us | 25.424 us |
| inspector entries_of | 34.144 us | 30.576 us |
| inspector artifact_row | 9.104 us | 8.736 us |
| rescan wall (1,000 changed) | 5.134 s | 5.151 s |
| refresh wall | 26.137 ms | 27.493 ms |
| rebuild wall over a published index | 9.434 s | 7.847 s |
| peak allocated while an index was replaced | 2,476,257,280 | 1,463,869,440 |

The disk rows and the payload row are the change, and so are the scan and the two rebuilds.
The scan is 22.7 per cent faster because there are fewer bytes to serialize and write, a
million change-log rows are no longer among them, and a unit is prepared outside the write
transaction. The first rebuild is 44.9 per cent faster because the artifact record it already
holds is no longer looked up and decoded a second time, there are fewer search columns to
write, and publishing no longer prunes a million log rows.

`node:self` moved from 474.983 ms to 408.324 ms, and read 352.126 and 438.563 ms at
`2c6bde6`. The direction is right and the mechanism is that six keyword columns no longer
exist for the collapse to open, but a spread of 86 ms across the after runs means the size is
not established.

The four other queries, all three inspector rows, the rescan and the refresh moved by less
than the spread of their own runs and are not results.

`peak rss growth during rescan and refresh` is omitted. It is a high-water mark attributed to
a phase by subtraction and read 65,220,608, 313,569,280, 95,752,192 and 72,101,888 bytes
across four runs of two commits; a 4.8x spread measures the allocator, not the change.

**What the change log cost.** At `2c6bde6` a fresh scan wrote one `SearchChange` row per
artifact and the rebuild that followed deleted every one of them unread, because a build
reads the artifact records rather than the log. At 1M the table was 32,000,000 stored bytes
over 80,171,008 bytes of pages, and redb does not return freed pages to the operating system,
so the file kept them. `1783f2b` writes none: allocated disk after the rebuild went from
1,480,302,592 to 1,399,648,256 bytes, 80,654,336 less, and the residual `catalog.redb` holds
beyond its tables fell from 87.68 MiB to 11.19 MiB. At 100k the same fix is worth 7,655,424
bytes.

At 100k, the same pair: the whole directory 243,363,840 to 143,675,392 bytes, `catalog.redb`
235,188,224 to 137,908,224, the search directory 8,175,616 to 5,767,168, record payload
103,123,417 to 55,512,438, scan 6.461 s to 5.019 s, rebuild 1.248 s to 0.836 s. The 100k
queries and inspector rows all moved by less than a millisecond and are not results.

Allocated disk is `du` over the output directory; apparent disk sums file lengths, and redb
grows `catalog.redb` in regions, so the apparent number is quantised and the allocated one is
what the disk actually loses.

### A page anywhere in the order, at 1M

The paging wave, measured in one session on this workstation over one tree, on an otherwise
idle machine. All three are checkpoints of the `wave/page-anywhere` branch that `master`
carries as one commit: `55330c2` is the harness with its new rows on the previous search
code, `d91302a` is the sampled selection as first written, and `2552309` is it after the
performance review that followed.

| measure | 55330c2 | d91302a | 2552309 |
| --- | --- | --- | --- |
| segments after rebuild | 3 | 3 | 3 |
| query empty by name | 119.410 ms | 124.878 ms | 121.149 ms |
| query deep offset | 119.454 ms | 125.401 ms | 120.742 ms |
| query midpoint offset | 1.508 s | 204.113 ms | 184.532 ms |
| query midpoint offset by size | 225.749 ms | 173.865 ms | 170.737 ms |
| query midpoint offset after refresh | 1.544 s | 216.370 ms | 189.565 ms |
| segments after refresh | 4 | 4 | 4 |
| query role:lora with facets | 141.857 ms | 149.102 ms | 146.527 ms |
| query filtered | 229.220 ms | 240.957 ms | 236.601 ms |
| query node:self with facets | 411.839 ms | 381.425 ms | 322.365 ms |
| resident rss over the queries | 17,113,088 (16.32 MiB) | 16,314,368 (15.56 MiB) | 16,314,368 (15.56 MiB) |
| resident rss over the midpoint queries | 61,407,232 (58.56 MiB) | 13,754,368 (13.12 MiB) | 14,278,656 (13.62 MiB) |
| peak rss growth during the queries | 0 | 0 | 0 |
| scan wall | 75.003 s | 75.241 s | 76.749 s |
| rebuild wall | 7.752 s | 8.251 s | 7.343 s |
| on disk after rebuild, allocated | 1,398,964,224 | 1,398,095,872 | 1,398,759,424 |

The three midpoint rows and the resident memory over them are the change. A page in the
middle of the default order fell from 1.508 s to 184.5 ms, 8.2x, and the same page over the
four segments a refresh leaves fell from 1.544 s to 189.6 ms; the memory the two midpoints
hold above their own page fell from 58.56 MiB to 13.62 MiB. The midpoint by size fell from
225.7 ms to 170.7 ms, a smaller effect because that key never spelled a term: it paid only
for the heap, and now pays for two extra passes instead.

Two costs the numbers separate. The 1.5 s was not the heap: sorting by name over three
segments spelled a term for every one of the 500,050 rows the heap held, and that spelling
was most of it, which is why the midpoint by size was 225 ms on the same window. The sampled
selection removes both, because the bucket it spells from holds about a thousand rows rather
than half the match.

What the review's four fixes were worth, `d91302a` to `2552309`: 19.6 ms off the name
midpoint and 26.8 ms off the same page after a refresh, which is the sample falling from a
fixed 4,096 boundaries to the square root of the match, 1,000 of them, each of which had cost
a dictionary seek in every segment that did not hold its term. The gap between the two
midpoints, 12.3 ms at `d91302a` against 5.0 ms at `2552309`, is that per-segment seek cost
and it shrank with the sample. The rest of the fixes, transposing the boundaries into one
contiguous table per segment and reading only the sort column and the artifact id in the
bucket pass, are inside the 184.5 ms and were not separately timed.

`query empty by name`, `query deep offset`, `role:lora` and the filtered query took 5 to
12 ms at `d91302a` that the review recovered most but not all of: against `55330c2` they now
stand at +1.7, +1.3, +4.7 and +7.4 ms. The first two are inside the spread of their own runs;
the last two are not, and are a 3.2 per cent regression on two rows this wave did not put on
a new path. All four are shallow windows that still take the heap. At 100k the same four rows
are back on their before numbers to within 0.7 ms, so whatever is left does not scale with
the change; the reviewer refuted the three mechanical explanations. Codegen around the
collector's inner loop is the hypothesis that remains, and it is only that: no profile has
been taken that establishes it.

`node:self` is the collapse path, which this wave did not touch; it read 411.839, 381.425 and
322.365 ms across the three columns, a spread wider than any difference between them. The
scan, rebuild and disk rows are the same workload as before and moved by less than their own
spread.

At 100k, `55330c2` to `2552309`: `query midpoint offset` 104.014 ms to 19.398 ms, `query
midpoint offset by size` 15.564 ms to 14.623 ms, `query midpoint offset after refresh`
111.448 ms to 20.654 ms, resident memory over the midpoints 1.09 MiB to 1.17 MiB. The 100k
index is two segments after the rebuild and three after the refresh, and a 100k midpoint by
size is a window of 50,050 rows, which is inside the numeric crossover below and so still
takes the heap. Nothing else at 100k moved by more than a millisecond.

**The sample must not resonate with the order documents were written in.** The stride was
`counted / boundaries`, an even number for every round match, and the synthetic tree writes
files whose size repeats with a period of 32. A stride of 1,000 shares a factor of 8 with
that period, so the sample only ever landed on four of the 32 size classes and 28 of them
got no boundary at all: with 1,002 boundaries the median bucket held 126 rows and the largest
held 80,835, over the 65,536 cap, so a size-sorted page at some offsets silently fell back to
the heap and paid for both. Forcing the stride odd makes it coprime with every power of two;
the same measurement then reads a median of 657 and a largest bucket of 8,619, and the four
size-sorted crossover offsets that had read 177.0, 177.6, 331.5 and 322.1 ms read 175.9,
176.1, 175.4 and 174.5 ms. Nothing about an answer depended on this - the equality tests hold
either way - only what it cost.

### Tallying by ordinal and reading by block

The tally wave, measured in one session on this workstation over one tree, on an otherwise
idle machine, with nothing else running. `95d6fe0` is the head the wave started from,
`d6897a2` is the per-segment vector tallies, `286784c` is those plus the block reads, and
`e2e1809` is the head after the performance review that followed: blocks that survive
deletes, and the collapse, window and bucket collectors blocked as well. The two 1M columns
either side of the review are the first of two runs each; `e2e1809` is one run.

| measure, 1M | 95d6fe0 | 286784c | e2e1809 |
| --- | --- | --- | --- |
| segments after rebuild | 3 | 3 | 3 |
| query empty by name | 120.306 ms | 21.153 ms | 21.143 ms |
| query deep offset | 120.761 ms | 21.436 ms | 21.448 ms |
| query midpoint offset | 184.543 ms | 86.762 ms | 71.963 ms |
| query midpoint offset by size | 170.409 ms | 72.186 ms | 58.298 ms |
| query role:lora with facets | 146.224 ms | 24.106 ms | 24.182 ms |
| query filtered | 234.624 ms | 38.370 ms | 38.341 ms |
| query node:self with facets | 338.563 ms | 178.317 ms | 120.578 ms |
| query midpoint offset after refresh | 189.095 ms | 110.939 ms | 76.649 ms |
| segments after refresh | 4 | 4 | 4 |
| resident rss over the queries | 56,238,080 (53.63 MiB) | 56,242,176 (53.64 MiB) | 16,314,368 (15.56 MiB) |
| resident rss over the midpoint queries | 14,278,656 (13.62 MiB) | 14,278,656 (13.62 MiB) | 14,278,656 (13.62 MiB) |
| peak rss growth during the queries | 0 | 0 | 0 |
| scan wall | 74.261 s | 77.286 s | 75.719 s |
| rebuild wall | 7.516 s | 8.799 s | 7.627 s |
| on disk after rebuild, allocated | 1,399,234,560 | 1,399,762,944 | 1,398,345,728 |

| measure, 100k | 95d6fe0 | d6897a2 | 286784c | e2e1809 |
| --- | --- | --- | --- | --- |
| query empty by name | 12.569 ms | 3.935 ms | 2.734 ms | 2.711 ms |
| query deep offset | 12.822 ms | 4.209 ms | 2.973 ms | 2.951 ms |
| query midpoint offset | 19.101 ms | 10.782 ms | 9.462 ms | 7.980 ms |
| query midpoint offset by size | 14.441 ms | 5.849 ms | 4.773 ms | 4.617 ms |
| query role:lora with facets | 14.995 ms | 4.271 ms | 2.878 ms | 2.844 ms |
| query filtered | 23.596 ms | 6.553 ms | 4.203 ms | 4.164 ms |
| query node:self with facets | 23.636 ms | 7.439 ms | 7.907 ms | 6.333 ms |
| query midpoint offset after refresh | 20.478 ms | 12.023 ms | 13.286 ms | 9.336 ms |
| resident rss over the queries | 1.93 MiB | 1.90 MiB | 1.84 MiB | 1.86 MiB |

Every query row is the change. At 1M the first page of the default order fell from 120.3 ms
to 21.1 ms, 5.7x; the deep page with it; `role:lora` with its facet rail from 146.2 ms to
24.2 ms, 6.0x; the filtered query from 234.6 ms to 38.3 ms; the middle page from 184.5 ms to
72.0 ms; and `node:self`, the collapse path, from 338.6 ms to 120.6 ms, 2.8x. The 100k
columns split the mechanisms: of the 9.8 ms the first page lost, the per-segment vectors took
8.6 ms and the block reads 1.2 ms more.

**A block read is only worth anything if it survives deletes.** At `286784c` the collectors
overrode `SegmentCollector::collect_block` and nothing else, which tantivy calls only while a
segment has no delete file. `refresh_search` deletes an artifact's documents by term before
re-adding them, so the first refresh puts every touched segment on the per-document path -
where `collect` now cost *more* than the code it replaced, because one document went through
ten `clear`, `resize(1)` and `first_vals` calls on a one-element slice. The 1M row that shows
it is the midpoint after a refresh, which sat 4.6 ms above the plain midpoint before the wave,
24.2 ms above it at `286784c`, and 4.7 ms above it again at `e2e1809`. Overriding
`Collector::collect_segment` and doing the alive filtering there restores the block path on
segments with deletes and is worth 34.3 ms of that row at 1M and 4.0 ms at 100k.

Blocking the other three collectors is the rest of the review: the plain midpoint fell
14.8 ms and the midpoint by size 13.9 ms, which is the bucket and window passes, and
`node:self` fell 57.7 ms, which is the collapse collector reading four columns per document
before. `query empty by name`, `query deep offset`, `role:lora` and the filtered query are
unmoved either way, inside 0.1 ms, because they never take those passes.

The 1M spread between the two runs at each commit: at `95d6fe0`, 119.4 to 120.3, 119.4 to
120.8, 182.8 to 184.5, 168.5 to 170.4, 144.7 to 146.2, 232.5 to 234.6 and 320.3 to 338.6 ms;
at `286784c`, 21.0 to 21.2, 21.3 to 21.4, 85.6 to 86.8, 71.3 to 72.2, 23.7 to 24.1, 37.8 to
38.4 and 173.1 to 178.3 ms. Every difference above is far outside both.

Resident memory over the queries reads 53.63 and 15.56 MiB at `95d6fe0`, 53.64 and 53.66 MiB
at `286784c`, and 15.56 MiB at `e2e1809`: the same two values on both sides of the change. The
row is a `VmRSS` difference across the five queries and is dominated by how much of the search
directory the page cache has faulted in, so its own spread is 38 MiB wide and it measures
nothing about this wave. What the wave adds is bounded and countable: nine buffers of at most
64 slots per segment collector, about 9 KB per segment, plus one `u64` per term of each facet
dictionary per segment.

The scan, rebuild and disk rows are the same workload as before and are not on any path this
wave touches: a rebuild writes documents and runs no collector. Its wall read 7.627 to
8.799 s across the three columns against 7.108 to 7.516 s at `95d6fe0`, a difference the size
of the spread of its own runs. One row moved for a reason the change cannot reach: peak RSS
growth during the rescan and refresh read 227.98 MiB at `e2e1809` against 55.32 and 56.52 MiB
before it. Neither a rescan nor a refresh runs a collector; the row is a high-water-mark
difference measured after the queries, so a run that faulted less of the index in during the
queries leaves the refresh more room to raise the mark. The absolute peak, the 864 MiB the
scan grows by, is the same in every run.

### The remaining work measured once, at `7bb681f`

Waves 1 to 7 of `docs/superpowers/specs/2026-09-06-remaining-work-goal.md` changed the query
paths, the scan, the readers and the record shapes, and none of them measured. This is the
one measurement of all of it: `3a9fa1f`, the commit the goal was set at, against `7bb681f`,
the head after wave 7, both built release in one session on this workstation, both run over
one tree that was generated once and its headers restored between runs, one process at a
time with nothing else on the disk. Two runs per column at 1M; the column is the first run
and the second is beside it.

| measure, 1M | `3a9fa1f` | run 2 | `7bb681f` | run 2 |
| --- | --- | --- | --- | --- |
| scan wall | 74.838 s | 76.788 s | 76.954 s | 76.003 s |
| peak rss growth during scan | 910,503,936 (868.32 MiB) | 911,605,760 | 901,083,136 (859.34 MiB) | 908,177,408 |
| rebuild wall | 7.459 s | 7.488 s | 7.995 s | 8.840 s |
| peak rss growth during rebuild | 344,657,920 (328.69 MiB) | 345,051,136 | 345,157,632 (329.17 MiB) | 344,354,816 |
| on disk after scan, allocated | 1,340,702,720 | 1,340,780,544 | 1,338,777,600 | 1,338,748,928 |
| record payload after scan | 563,970,209 (537.84 MiB) | 563,970,982 | 561,970,119 (535.94 MiB) | 561,970,286 |
| on disk after rebuild, allocated | 1,399,160,832 (1334.34 MiB) | 1,399,238,656 | 1,397,235,712 (1332.51 MiB) | 1,397,207,040 |
| of which the search directory | 58,458,112 | 58,458,112 | 58,458,112 | 58,458,112 |
| segments after rebuild | 3 | 3 | 3 | 3 |
| query empty by name | 20.941 ms | 20.791 ms | 21.524 ms | 21.633 ms |
| query deep offset | 21.337 ms | 21.230 ms | 38.807 ms | 38.860 ms |
| query midpoint offset | 71.924 ms | 71.925 ms | 72.995 ms | 72.991 ms |
| query midpoint offset by size | 58.185 ms | 58.197 ms | 59.376 ms | 59.514 ms |
| query role:lora with facets | 24.028 ms | 23.881 ms | 24.759 ms | 24.819 ms |
| query filtered | 38.206 ms | 37.908 ms | 39.585 ms | 39.572 ms |
| query node:self with facets | 198.184 ms | 200.063 ms | 50.887 ms | 50.713 ms |
| query node:self midpoint offset | not in the harness | | 107.071 ms | 107.019 ms |
| query node:self deep offset | not in the harness | | 74.765 ms | 74.775 ms |
| query midpoint offset after refresh | 76.582 ms | 76.323 ms | 76.386 ms | 76.693 ms |
| segments after refresh | 4 | 4 | 4 | 4 |
| inspector artifact_detail | 26.704 us | 26.929 us | 26.688 us | 26.559 us |
| inspector entries_of | 27.744 us | 28.880 us | 29.776 us | 30.688 us |
| inspector artifact_row | 8.496 us | 8.832 us | 8.816 us | 8.608 us |
| resident rss over the queries | 13,770,752 (13.13 MiB) | 16,314,368 | 16,322,560 (15.57 MiB) | 16,388,096 |
| resident rss over the midpoint queries | 14,315,520 (13.65 MiB) | 14,278,656 | 14,278,656 (13.62 MiB) | 14,278,656 |
| resident rss over the collapse queries | not in the harness | | 0 | 0 |
| rescan wall (1,000 changed) | 5.237 s | 5.238 s | 5.068 s | 5.063 s |
| refresh wall | 29.302 ms | 29.046 ms | 27.598 ms | 22.318 ms |
| on disk after churn, allocated | 1,403,031,552 | 1,403,002,880 | 1,400,991,744 | 1,401,040,896 |
| rebuild wall over a published index | 8.256 s | 7.681 s | 7.490 s | 7.360 s |
| peak allocated while an index was replaced | 1,465,954,304 | 1,464,471,552 | 1,461,444,608 | 1,461,485,568 |

| measure, 100k | `3a9fa1f` | `7bb681f` | `7bb681f` again |
| --- | --- | --- | --- |
| scan wall | 5.185 s | 5.215 s | 5.176 s |
| rebuild wall | 861.5 ms | 838.9 ms | 709.8 ms |
| on disk after rebuild, allocated | 143,687,680 | 142,630,912 | 142,639,104 |
| record payload after rebuild | 55,512,643 | 55,312,343 | 55,312,202 |
| query empty by name | 2.720 ms | 2.647 ms | 2.618 ms |
| query deep offset | 2.958 ms | 4.684 ms | 4.658 ms |
| query midpoint offset | 7.987 ms | 8.073 ms | 8.051 ms |
| query midpoint offset by size | 4.695 ms | 4.791 ms | 4.694 ms |
| query role:lora with facets | 2.875 ms | 2.936 ms | 2.908 ms |
| query filtered | 4.182 ms | 4.324 ms | 4.345 ms |
| query node:self with facets | 6.900 ms | 5.398 ms | 5.379 ms |
| query node:self midpoint offset | not in the harness | 11.446 ms | 11.302 ms |
| query node:self deep offset | not in the harness | 8.236 ms | 8.157 ms |
| query midpoint offset after refresh | 9.328 ms | 9.298 ms | 9.353 ms |

**The collapse path is the result.** `node:self` at 1M fell from 198.184 to 50.887 ms, 74.3
per cent, against a spread of 1.9 ms in the before column and 0.2 ms in the after one, which
is wave 4 folding an artifact's run of entry documents inside the segment collectors instead
of retaining a row per matching artifact. Its two new pages, which no column before this one
could report, cost 107.071 ms in the middle of the order and 74.765 ms at the far end, and
the resident memory over the two of them is zero. At 100k the same row is 6.900 to 5.398 ms.

**A deep page pays a sampling pass it did not pay before.** `query deep offset` at 1M went
from 21.337 to 38.807 ms and at 100k from 2.958 to 4.684 ms, in both runs of both columns.
Wave 4 removed the `Count` pass a primary query ran before anything else, so the total is
now learned from the sampling pass, and only then does `Window::of` discover that the page
is nearer the far end and a reversed heap of 100 rows will answer it. Before, a `Count` that
read no column bought that knowledge; now a sampling pass that reads the sort column does.
This is exactly the trade named under "Remaining work" as the price of dropping the `Count`,
now with its number: 17.5 ms at 1M on the last page of a million, which is 12.9 times inside
the 500 ms gate.

**Everything else is inside its own spread or nearly so.** The scan, both rebuilds, the
rescan, the refresh, the three inspector rows and the disk rows moved by less than the
difference between the two runs of a single column. `query empty by name`, `query midpoint
offset`, `query midpoint offset by size`, `role:lora` and the filtered query are 0.6 to
1.4 ms slower at 1M, 1.5 to 3.6 per cent, consistently across both runs of both columns and
therefore just outside a spread of 0.2 ms; nothing in these waves put those rows on a new
path and no mechanism is established for them. At 100k the same five rows are within 0.15 ms
either way, so whatever it is does not scale with the entry count. This is the same shape of
residual the paging wave recorded and left, and it is recorded and left again.

**The disk rows barely moved and were not expected to.** The disk wave's three levers - one
card per revision, the remaining records on the compact codec, a smaller `ArtifactDetail`
summary - need cards, many revisions and many tensor samples to show anything, and the gate
fixture has one tensor per file, one revision per repository and no card at all. `SearchText`
lost 1,000,000 bytes of payload and `ArtifactRecord` 1,000,306; `ArtifactDetail` is identical
to the byte. What the wave was worth is in the supplemental fixture below, where the whole
directory fell 12.3 per cent.

**One row moved for a reason none of these waves can reach.** Peak RSS growth during the
rescan and refresh reads 57,901,056 and 57,593,856 bytes before and 140,234,752 and
123,826,176 after. It is a high-water-mark difference attributed to a phase by subtraction,
and the after column runs two more queries before it than the before column does, so the
mark it is measured from is set differently. The absolute peak, the 866 MiB the scan grows
by, is the same in every run of both columns.

### Three workloads the harness had never run, at 1M

Three phases the scale harness did not have before `2403264`, each behind its own
environment variable and skipped when it is unset, so a run without them prints exactly what
the run printed at `0935fde`: a mass deletion, eight concurrent midpoint queries, and a usage
log of half a million rows. They exist because the run of 2026-09-07 rewrote four subsystems
for memory without measuring any of them; these are the three workloads the project owner
named as the gaps. **This wave changed no production code and proposes no fix.**

```sh
HOARD_SCALE_ENTRIES=1000000 HOARD_SCALE_DIR=<scratch>/scale \
  HOARD_SCALE_CONCURRENT=1 HOARD_SCALE_USAGE=1 HOARD_SCALE_DELETE=1 \
  cargo test --release -p hoard-index --test scale -- --nocapture --ignored
```

Both columns are the harness at `2403264` on this workstation, two runs of one session over
one tree, nothing else running, no `RUSTFLAGS`. The phases run in the order below, after the
existing incremental workload and before the tree is put back: the concurrent queries first,
so they read the same index state the single-threaded midpoint row above them read, and the
deletion last, because it is the only phase that changes what the catalog holds.

Each phase measures its peak against a mark it sets itself, by writing `5` to
`/proc/self/clear_refs`, which lowers `VmHWM` to what is resident at that moment. Without it
every one of these rows would read zero, because the scan's own 830 MiB peak is never
exceeded again in the process. The resident row beside each peak is `VmRSS` before against
after, which is what a process doing only that work would still hold.

| measure | first run | second run |
| --- | --- | --- |
| eight concurrent midpoint queries, offset | 499,950 | 499,950 |
| slowest of the eight | 632.547 ms | 681.280 ms |
| wall from spawn to join | 633.047 ms | 681.684 ms |
| the same page single-threaded, same run | 72.006 ms | 72.266 ms |
| peak rss growth over the eight | 19,296,256 (18.40 MiB) | 20,193,280 (19.26 MiB) |
| resident rss over the eight | 16,252,928 (15.50 MiB) | 16,232,448 (15.48 MiB) |
| repositories the usage rows name | 1 | 1 |
| usage rows seeded, in batches of 256 | 500,000 | 500,000 |
| seed wall | 5.203 s | 5.171 s |
| peak rss growth while the log was seeded | 0 | 0 |
| `recent_pulls` at 20 | 163.689 ms | 172.482 ms |
| peak rss growth over `recent_pulls` | 70,823,936 (67.54 MiB) | 71,512,064 (68.20 MiB) |
| resident rss over `recent_pulls` | 25,034,752 (23.88 MiB) | 25,034,752 (23.88 MiB) |
| `usage_report` at 7 days and 20 rows | 146.091 ms | 149.435 ms |
| peak rss growth over `usage_report` | 45,723,648 (43.61 MiB) | 46,342,144 (44.20 MiB) |
| `usage_days` at 90 days | 427.532 ms | 442.417 ms |
| peak rss growth over `usage_days` | 119,779,328 (114.23 MiB) | 118,431,744 (112.95 MiB) |
| pulls the 90 day chart counted | 500,000 | 500,000 |
| directories deleted, of 1,000 | 100 | 100 |
| files deleted | 100,000 | 100,000 |
| rescan wall with a tenth of the tree gone | 16.573 s | 16.942 s |
| the same walk with 1,000 changed, same run | 4.783 s | 4.901 s |
| peak rss growth during the deletion rescan | 399,179,776 (380.69 MiB) | 406,638,592 (387.80 MiB) |
| resident rss over the deletion rescan | 42,807,296 (40.82 MiB) | 41,693,184 (39.76 MiB) |
| `absent` the deletion rescan reported | 100,000 | 100,000 |
| files in the tree after it was restored | 1,000,000 | 1,000,000 |
| scan wall, same run | 76.085 s | 73.127 s |
| peak rss growth during the scan, same run | 873,218,048 (832.77 MiB) | 871,112,704 (830.76 MiB) |

**A rescan that finds 100,000 files gone: no problem found.** It reports exactly the 100,000
it should, retires them without an interrupted walk, and grows the process by 380.7 and
387.8 MiB against a gate of 2 GB and against the 832.8 MiB the full scan in the same run
grows by. The wall is 16.6 and 16.9 s, against 4.8 and 4.9 s for the same walk of the same
tree when 1,000 files changed and none was missing, so retiring 100,000 entries costs about
12 s, roughly 120 microseconds each. No gate is written for a rescan's wall.

**Eight concurrent midpoint queries: no problem found on memory; the latency is a number to
read carefully.** All eight together grow the process by 18.40 and 19.26 MiB, against the
13.62 MiB of resident memory one midpoint page holds on its own in the rows above: aggregate
memory across concurrent expensive searches is bounded, which is what the workload was built
to find out. The slowest of the eight took 632.5 and 681.3 ms, against 72.0 and 72.3 ms for
the same page single-threaded in the same run. Eight times 72 ms is 576 ms, so the eight are
close to serialized, and the slowest one is over the standing 500 ms query gate while the
single-request row is 427 ms inside it. That gate is written for a query, and nothing in this
document says whether it is meant to bind on the slowest of eight at once; this wave measured
the number and takes no view on the mechanism, which no measurement here establishes.

**A usage log of 500,000 rows: no problem found against the gates.** All three reads the
USAGE tab makes are under 500 ms, the widest of them, the 90 day chart that is counted from
the log rather than from the repository's own record, by 57.5 and 72.5 ms. Nothing in the
phase approaches the memory gate: the largest is 114.23 MiB over that chart.

Three facts about that log which are not verdicts. The gate fixture is one repository, not a
hundred, because `config` in the harness sets `repo_per_folder: false` and loose discovery
then groups every file under one repository, so the skew this phase was written for has
nowhere to spread and the one repository holds all 500,000 rows. `recent_pulls` returns 20
rows and grows the process by 67.5 and 68.2 MiB doing it. The same three reads over the same
500,000 row log on a tree of 5,000 files took 145.3, 133.1 and 408.5 ms, so what they cost is
in the length of the log and not in the size of the catalog; their peak growth over that
smaller tree read 90.07, 44.10 and 208.79 MiB, which is not comparable to the column above,
because a process that has just scanned a million files has an allocator that can serve the
same work without asking the kernel for more.

Seeding grew the process by zero, which is not evidence that seeding allocates nothing: the
peak mark is set after a scan and a rebuild have already run, and 500,000 rows written in
1,954 transactions fit inside what those left behind.

### Thresholds

At 1M entries, absolute: peak RSS growth during the scan under 2 GB, allocated disk under
1,500,000,000 bytes at rest and at the peak of a rebuild that replaces a published index,
every query under 500 ms including the collapse path and a page anywhere in the order, and a
rebuild no slower than the scan. Measured at `7bb681f` over the tree of the section above,
first run of two, with the second beside it where the two differ.

| gate | measured at `7bb681f` | verdict |
| --- | --- | --- |
| allocated disk after the rebuild | 1,397,235,712 bytes (1332.51 MiB) | met, by 102.8 MB |
| allocated disk after the incremental workload | 1,400,991,744 bytes | met, by 99.0 MB |
| allocated disk at the peak of a rebuild that replaces a published index | 1,461,485,568 bytes | met, by 38.5 MB |
| peak RSS growth during the scan | 908,177,408 bytes (866.11 MiB) | met, by 1.1 GB |
| rebuild no slower than the scan | 7.995 s against 76.954 s | met |
| every primary query under 500 ms | slowest is `query midpoint offset` at 72.995 ms | met, by 427.0 ms |
| the collapse queries under 500 ms | 50.887, 107.071 and 74.765 ms | met, by 392.9 ms |
| every page of the default order under 500 ms | 21.524 first, 72.995 middle, 38.807 last, 76.693 after a refresh | met, by 423.3 ms |
| no route of the eleven slower than the column beside it | the model route +1.76 ms, the default search page +1.87 ms, the usage route +0.82 ms | **missed** on three, two of them unexplained |

Wave 9 answered that last row and the deep page beside it, over the same trees, at the head:

| gate | measured at `2c866f5` | verdict |
| --- | --- | --- |
| the last page of the default order at 1M | 21.453 ms, against 21.405 ms at `3a9fa1f` and 38.566 ms at `7a57762` | met; the row wave 8 missed |
| no route slower than the column beside it | paired and warm, the model route -0.99 ms, the search page -0.10 ms and the usage route -0.45 ms against `3a9fa1f`, with the `/healthz` control flat | met; the two unexplained rows were the protocol, not the code |
| an orderly shutdown does not wait for a hashing pass | 0.040 s from SIGTERM to exit with the reconciliation's hashing in flight, against about 4.5 minutes | met |

The disk gate's margin is the number to watch and it has not moved: the resting figure is
102.8 MB inside it, and a rebuild that replaces a published index holds two search
directories for as long as it runs and comes to 38.5 MB inside it. That transient is one
search directory, 58,458,112 bytes, above the resting figure.

This table replaces the per-wave gate tables the disk, paging and tally waves each wrote
here against the commit their own wave started from. Their measurements are unchanged and
stay in "The scale harness", "A page anywhere in the order, at 1M" and "Tallying by ordinal
and reading by block" above; the gates themselves are answered once, here, at the head.

**Where the heap stops paying.** `Paging::DEFAULT.shallow` is the window above which a rank
selection replaces the heap, and it is 16,384 rows for a text key and eight times that for a
numeric one. Measured at 1M on the tree above with two release builds of the harness, one
forced to the heap and one forced to the sample, each run once, each offset timed warm after
the fixed list had already run; these numbers are therefore not comparable to the table
above, only to each other.

| window | heap, text | sampled, text | heap, numeric | sampled, numeric |
| --- | --- | --- | --- | --- |
| 8,292 | 147.209 ms | 189.469 ms | 127.316 ms | 175.926 ms |
| 32,868 | 228.059 ms | 189.610 ms | 140.856 ms | 176.105 ms |
| 131,172 | 476.414 ms | 188.873 ms | 175.330 ms | 175.431 ms |
| 500,050 | 1.386 s | 186.409 ms | 141.718 ms | 174.519 ms |

The sampled columns are flat, which is the property the wave was for: what a page costs does
not depend on where in the order it sits. The heap column is what crosses them. A text key
crosses between 8,292 and 32,868, at about 20,900 rows by interpolation, because the heap
spells a term for every row it keeps; 16,384 is the power of two under that. A numeric key
compares integers and does not cross until 131,172, where the two are level to 0.1 ms;
131,072 is the constant, and `NUMERIC_SHALLOW` is the factor of 8 between them.

Past that point the numeric heap is not monotone - it reads 175.330 ms at 131,172 and
141.718 ms at 500,050 - so at the midpoint a numeric heap is about 33 ms faster than the
sample on a single warm sample. It is not chosen there: that heap holds 500,050 rows in every
segment it reads, which is the 58.56 MiB the wave removed, against about a thousand rows for
the bucket. Where the two are level in time the bounded one is taken - except below the caps,
where the sampled selection falls back to the heap on purpose, so paging is bounded
everywhere above them and not below.

The first two rows are the catalog at rest. The third is not: `publish` writes the new
`search-v1-<nanos>` directory in full and keeps the old one until the new one is committed
and named in the manifest, so for the length of a rebuild the disk holds two search
directories. That transient is one search directory, 58,458,112 bytes, above the resting
figure, and it is the number to watch, because it was **over** the gate at 1,539,854,336 and
1,541,668,864 bytes one commit earlier and only came under it when the change log stopped
being written. Nothing here redesigns `publish`: an unpublished build leaving the previous
one serving is what makes a failed rebuild harmless.

The disk gate was missed by 887,607,552 bytes at `95cd3ad` and is met by 100.4 MB at
`1783f2b`, a 41.4 per cent reduction.

Standalone query-process resident memory over the five queries is 71.50 MiB, against zero
high-water growth: a query allocates, the process had simply already been higher during the
rebuild. Zero high-water growth after a scan is not proof that a query allocates nothing.

### Where the bytes are

Measured with `crates/hoard-index/tests/scale.rs`, which closes the catalog and reads redb's
own per-table statistics after the scan, after the rebuild and after the incremental
workload, and writes each report to `storage-<entries>-<phase>.json` beside the catalog.

Three numbers per table, and they mean different things:

- **stored** is the key and value payload of the rows, which is what a record encoding
  changes.
- **metadata** is the branch keys and page headers around it. For a secondary index the
  stored column is only the 8 byte primary keys it points at, so the index key itself is
  counted here.
- **pages** is `(leaf + branch) * 4096`, the space the file actually spends. Fragmentation
  is the difference: unused room inside pages redb has already allocated, mostly ordinary
  B-tree fill factor rather than garbage a compaction is promised to return. Every large
  table sits near 45 per cent fragmented straight after a fresh scan, which compacts, and
  that ratio did not move: halving a payload halves the pages, it does not pack them
  tighter.

At 1M entries, after the rebuild, at `3a9fa1f` and `7bb681f`, both taken in one session on
this workstation with the same harness over the same tree.

| table | rows | pages at `3a9fa1f` | pages at `7bb681f` | stored at `7bb681f` | leaf fill |
| --- | --- | --- | --- | --- | --- |
| `8_1_artifact_id`, `ArtifactDetail` | 1,000,000 | 497.00 MiB | 497.00 MiB | 267.52 MiB | 53.8% |
| `3_1_id`, `ArtifactRecord` | 1,000,000 | 278.11 MiB | 278.18 MiB | 135.15 MiB | 48.6% |
| `7_1_id`, `Entry` | 1,000,000 | 164.78 MiB | 164.78 MiB | 75.00 MiB | 45.5% |
| `7_1_location`, the unique location index | 1,000,000 | 118.08 MiB | 118.08 MiB | 7.63 MiB | 6.5% |
| `5_1_artifact_id`, `SearchText` | 1,000,000 | 88.23 MiB | 86.31 MiB | 35.37 MiB | 41.0% |
| `7_1_artifact_id`, the artifact index | 1,000,000 | 63.11 MiB | 63.11 MiB | 7.63 MiB | 12.1% |
| `3_1_identity`, the identity index | 1,000,000 | 58.11 MiB | 58.11 MiB | 7.63 MiB | 13.1% |

The seven tables came to 1,328,984,064 bytes of pages and now come to 1,327,046,656, against
1,338,748,928 bytes allocated for `catalog.redb`: the 11.16 MiB residual is redb's own
allocator state, its freed-page tables and the region headers. The published Tantivy
directory is 58,458,112 bytes in both columns.

Nothing here is the disk wave's doing, because this fixture cannot show it: one tensor per
file leaves `ArtifactDetail` nothing to thin, one revision per repository and no card leave
`SearchText` nothing to move out. `SearchText` lost exactly 1,000,000 stored bytes and
`ArtifactRecord` 1,000,306: one byte a row off each, which is the size of `sniff_version`
gone from every artifact record and of the card field gone from every search text once the
card became a row of its own. `ArtifactDetail` is identical to the byte. What the wave was
worth is in the supplemental fixture below.

The largest remaining cost is `ArtifactDetail` at 497.00 MiB of pages, 38.9 per cent of
`catalog.redb`, of which 267.52 MiB is payload and 217.01 MiB is fill factor. The second is
`ArtifactRecord` at 278.18 MiB, 21.8 per cent.

**Intra-page slack is still the largest single term.** Across every table it is 608,456,492
bytes, 45.4 per cent of `catalog.redb`, exactly where it was at `3a9fa1f` at 608,374,746 and
45.4 per cent. `docs/superpowers/research/2026-09-page-packing.md` reads redb 2.6.3 and
answers why: one split rule that always cuts at the byte midpoint with no append case, so
insertion order, a bulk load in key order and a dense rewrite all land at the same fill,
`Database::compact` moves whole pages and never rebuilds a leaf, and `set_page_size` is
compiled only under `cfg(any(fuzzing, test))`. The reachable saving is 0 to 5 per cent. The
lever that works is payload, at about two bytes of file per byte off a row.

At 100k the same order and ratios hold: the whole directory is 142,630,912 bytes against
143,687,680 at `3a9fa1f`.

### What the one-artifact-per-file fixture cannot show

`supplemental_catalog_costs` seeds 200 repositories of 3 revisions of 50 files: long relative
paths, an hf-cache source whose entries carry an explicit blob path, populated sha256 and git
blob shas, a full model card and tensor summary per artifact, and one artifact in ten reached
by more than one path. 30,000 entries, 27,300 artifacts. It needs `--features fixtures`;
`-p hoard-index` alone leaves the module out and the command reports success over zero tests.

| measure | `3a9fa1f` | `7bb681f` |
| --- | --- | --- |
| whole directory, allocated | 134,848,512 bytes | 118,280,192 bytes |
| `catalog.redb`, allocated | 109,944,832 bytes | 93,298,688 bytes |
| `SearchText` stored | 17,335,378 bytes | 5,568,950 bytes |
| `RevisionText` stored | no such table | 287,597 bytes over 600 rows |
| `ArtifactDetail` stored | 17,971,722 bytes | 17,971,722 bytes |
| record payload | 53,444,433 bytes (50.97 MiB) | 41,896,878 bytes (39.96 MiB) |
| payload per entry | 1,781 bytes | 1,396 bytes |
| seed wall | 1.611 s | 1.795 s |
| rebuild wall | 1.266 s | 1.289 s |
| inspector `artifact_detail` | 24.5 and 49.2 us | 54.8 and 38.2 us |

One card per revision is what the disk wave bought and this is where it shows: 200
repositories of 3 revisions carry 600 cards rather than 27,300 copies of one, `SearchText`
falls to 32.1 per cent of what it stored, and the whole directory falls 16,568,320 bytes,
12.3 per cent. `ArtifactDetail` does not move, because this fixture's tensor summary is
already inside the sixteen samples the wave capped it at; the cap shows on a real header
with sixty-four. The inspector row was run twice per column and read 24.5 and 49.2 us
before against 54.8 and 38.2 us after: a spread wider than any difference between the
columns, so it says nothing either way.

The earlier reading of this fixture, at the commit before records were compact and the
search columns trimmed, is history: 311,848,960 bytes for the whole directory against
134,791,168 at `1783f2b`.

### Units

Runs before this one divided byte counts by 1024 squared and labelled the result MB. Every
number here is exact bytes with its MiB conversion beside it. The disk gate this document
tracks is stated in bytes: **1,500,000,000**. 1.5 GiB would be 1,610,612,736, so the two
readings of the old threshold are 110 MB apart and the byte form is the one used.

## Remaining work

- **A shallow window is never cut from the far end.** The pass that counts a shallow window
  is the pass that keeps it, and nothing knows the total before it runs, so the heap holds
  the `offset + limit` rows at the front of the order where the count-first selection held
  the `total - offset` rows at the back. A page just inside the shallow bound and past the
  middle of a small result therefore holds more than it did: at most the shallow constant,
  16,384 rows for a term key and 131,072 for a numeric one, against the two dozen a reversed
  heap would have held. The UI's cursor advances by `offset + limit`, so reaching such an
  offset means paging there or forging a cursor. Accepted as the price of dropping the
  `Count` pass, which every query paid. The fix, if a measurement ever asks for it, is to run
  the `Count` only when the offset is above some fraction of the shallow bound, so the
  common page keeps its one pass and the deep one buys its reversal. A measurement asked:
  at `7bb681f` the last page of a million cost 38.807 ms against 21.337 ms at `3a9fa1f`, and
  at 100k 4.684 against 2.958 ms. **Closed in wave 9**, and not by a fraction of the bound: a
  window inside the shallow bound is still one heap pass, and a window outside it pays one
  `Count` that reads no column and then decides. A window the count puts near the far end is
  a reversed heap and costs two passes, the cost it had before; a window in the middle pays
  the count, the sampling pass, the bucket pass and the window pass, four. The last page of a
  million is 21.453 ms again, and a page past the end costs the count plus the pass that
  tallies the facets rather than the sampling pass alone.
- **The index writer's pipeline is bounded in artifacts, not documents.** `build_index` and
  `refresh_search` send one `IndexWriter::run` group per artifact, which is what puts an
  artifact's documents in one segment and lets the query path fold them as a run. tantivy's
  operation channel is `bounded(PIPELINE_MAX_SIZE_IN_DOCS)`, 10,000 items, and an item was
  one document and is now one artifact's whole set, so the documents in flight ahead of the
  indexing thread scale with entries per artifact times the size of a document, which the
  revision card dominates: the card is stored once per revision but is still indexed into
  every document of it. The synthetic tree has few entries per artifact and a small card,
  so the harness cannot show this; read the rebuild RSS row with it in mind.
- **A collapsed pass reads a chosen row's facet ordinals one document at a time.** The
  primary path reads the five facet columns and the base label per block through
  `first_vals`; `HitColumns::hit_of` reads them per artifact through `first_ord` instead,
  because the entry a run chose may sit in a block already read past. It is six column reads
  per artifact rather than per matched entry, which is what the collapse cost before the
  fold, so it is not a regression. Left as it is.
- **A tally holds a slot per term of the column, not per term the match used.**
  `SegmentTallies::sized_for` allocates `num_terms()` slots per facet dimension per segment,
  which is eight bytes per base value per segment whether the query matched it or not. The
  five facet columns are small closed vocabularies except `base`, so a catalog with a hundred
  thousand distinct bases would allocate 800 KB per segment per pass where the old map
  allocated per value matched, and the review put the zeroing alone at about 12 ms per
  request at that cardinality. It is below the noise at any plausible base count today; it is
  the trade the vector makes and the number to watch if base cardinality ever grows.
- **The cap's fallback recollects rather than reusing the sampling pass.** When the chosen
  bucket is over 65,536 rows the heap answers the whole request, and the tallies and node set
  the sampling pass already computed are dropped and computed again. It is accepted as it
  stands: reaching it needs a bucket that dodged every sample slot, which the odd stride
  makes improbable, and the alternative is a second way to assemble a `Summary` for a path
  that should not run.
- **Pack the pages.** Intra-page slack is the largest single term left in `catalog.redb`:
  608,456,492 bytes at 1M at `7bb681f`, 45.4 per cent of the file, with the four record
  tables at 41 to 54 per cent leaf fill; it was 608,374,746 bytes and the same 45.4 per cent
  at `3a9fa1f`, so seven waves of work moved it by 0.01 per cent.
  `docs/superpowers/research/2026-09-page-packing.md` reads redb 2.6.3 and answers it: the
  fill is decided by one split rule that always cuts a leaf at the byte midpoint and has no
  append case, so insertion order does not touch it, a bulk load in key order buys nothing,
  `Database::compact` moves whole pages and never rebuilds a leaf, and
  `Builder::set_page_size` is `cfg(any(fuzzing, test))` and so is not in a release build at
  all. The recommendation there is to leave it: the reachable saving is 0 to 5 per cent, and
  the lever that works is payload, at about two bytes of file per byte off a row.
- **`sweep_artifact` decompresses a cold row to delete it.** native_db 0.8.2 has no
  remove-by-primary-key: `remove` takes the value, so the sweep reads `ArtifactDetail` and
  `SearchText` in full, LZ4 and bincode included, only to hand them straight back. Every
  artifact a pass orphans pays a decode it does not use. A `remove_by_key` upstream, or a
  cheaper path that reads the raw bytes, would take it off the rescan.
- **`native_model::Model::native_model_decode` unwraps** a payload shorter than its eight
  byte header rather than returning an error, so a truncated row panics. `codec.rs` cannot
  see that byte range; it is reached before any codec is called.
- **A path outside UTF-8 is identified but not stored.** The identity a scan computes is
  byte-exact, but `Repository.root_path` and `Entry.placed` hold `to_string_lossy` text, the
  walker drops a relative path that is not UTF-8, and `SourceConfig` cannot serialise a root
  that is not, so such a path never reaches a row. Storing the bytes is the remaining half.
- **A revision the walk no longer finds keeps its slot, and now nothing else.** The slot is
  what makes an entry's ordinal an identity, so it cannot be reclaimed while any entry names
  it. Wave 9 empties it instead: a revision no ref reaches and no entry names is written back
  as a tombstone and its `RevisionText` card is dropped in the same transaction, so a
  repository whose branch moves on every scan grows one empty slot per old head rather than a
  card, a commit and a title per old head. Reclaiming the slot itself still needs the entries
  of a retired revision to be removed rather than marked absent, which is the same decision
  as pruning a deleted path's row, and is not taken.
- **Add a read-only open** once redb offers one, so searches can run beside a rebuild.

## Dependency and serialization findings

`native_db 0.8.2` requires exactly `native_model 0.4.20` and resolves to `redb 2.6.3`. Legacy
native_db upgrade features are disabled. `tantivy 0.26.1` uses only mmap and LZ4 features.
These are intentional constraints, not latest-major assumptions.

Hoard's row ids previously always used Serde's `deserialize_any`, which bincode cannot decode.
A binary deserializer now reads a row id as an integer and a hash as bytes, and a
human-readable one keeps reading and writing the prefixed string and the hex; JSON also keeps
accepting an integer id. `bincode 1.3` and `lz4_flex 0.13` are direct dependencies of
`hoard-index` for `catalog/codec.rs`: bincode is the major version native_model 0.4.20
already resolves to, and lz4_flex is taken with `default-features = false` and the safe
encode, decode and checked-decode features rather than through Tantivy's copy of it. The
persistence and reopen tests exercise both codecs, and `codec.rs`'s own tests exercise what
they refuse.
