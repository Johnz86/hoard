# Native catalog reference

Hoard stores its catalog in native_db/redb and publishes a derived Tantivy search index.
This reference describes storage and write contracts. Source code and tests settle implementation
details; dated performance results live in [catalog measurements](../performance/catalog-measurements.md).

## Compatibility

Source removal reuses the existing bounded retirement path, derived counters, artifact sweep
and search outbox. It has no stored shape or format change. Runtime removal is serialized
with the coordinator scan lane; persisted server exclusions are replayed at startup. Finished
scan history uses a reserved `jobs.high_water` Setting so deleting job rows never reuses IDs,
including after reopening a catalog that predates that counter. History edits protect active
IDs under the admission gate; cancelled records can still belong to work the lane is finishing.

Git-project search adds no persisted record or format bump. `Catalog::git_repositories` and
`repository_by_id` expose existing source/repository metadata. The server builds a transient
`hoard_index::projects::ProjectIndex` over names, paths and bounded committed README text,
streams documents into a single in-memory Tantivy writer, and shares the resulting immutable
index between readers. A finished scan invalidates that cache. Project search does not
change the persisted artifact catalog or its search counters.

The [compatibility policy](../../CLAUDE.md#compatibility) defines persisted-format changes.

Derived search schemas have a separate lifecycle. `Catalog::open_or_scan` compares the
published Tantivy schema with `Fields::build()` and rebuilds a mismatch from catalog metadata.
This preserves source records, user-maintained state, hashes and allocated IDs; it is not a
catalog format bump or source rescan. See [search-schema upgrades](../operations/deployment.md#search-schema-upgrades).

## Layout

`<data_dir>/native/` holds:

- `FORMAT`, the catalog format as a decimal number and a newline, read before redb is opened.
- `catalog.redb`, the one file every record lives in.
- one published `search-v1-<nanos>/` directory holding the Tantivy index. A rebuild writes a
  new directory and publishes its name in a redb transaction only after Tantivy commits; an
  unpublished build leaves the previous one serving, and publishing removes every superseded
  directory.

Indexed resources can be rediscovered from sources and peers. Shelves, loadouts, verdicts
and usage are catalog state and require backups; rebuilding resources does not recreate them.

redb takes an exclusive lock, so one process may open a catalog at a time; `hoard doctor` reports it when a second one tries.

`<data_dir>/` also holds digest/lookup sidecars and server-managed JSON settings.
The memo files sit beside `native/` because `open_or_scan` answers a format mismatch with `remove_dir_all` of
`<data_dir>/native` alone, so what sits next to that directory survives a rebuild.

- `digests.redb`, the hashing pass's memo: one row per file it has read, keyed by a `u128`
  xxh3 of the inode, mtime and size that earned the digest and holding those three fields
  again so a hash collision degrades to a miss rather than a wrong answer, valued by the
  sha256 and, when it was computed, the git blob sha1. `Catalog::hash_pending_until` opens it
  for the length of one pass and closes it again, and does not open it at all when the pass is
  owed nothing; nothing holds it open. Before a candidate is queued to a worker the pass stats
  it and asks the memo, and a hit goes straight to the digest writer without the file being
  read. A row is a hit only when all three stamp fields match **and** it answers the same
  question about the git blob sha1 that this pass is asking: `[index] lfs_threshold` decides
  whether a git blob sha1 is computed at all, so an operator who moves that threshold gets the
  files re-read rather than the previous threshold's answer served back. The digests a batch
  read are written to the memo in one transaction after that batch's own catalog transaction
  commits, on the writer thread, because redb has no durability knob and fsyncs every commit;
  a recalled digest is not written back, since the memo is where it came from. Nothing else
  builds the memo: an ordinary pass fills it.
- `digests.version`, that memo's own format as a decimal number and a newline, versioned apart
  from `FORMAT`: a content digest and an inference rule are unrelated facts and a change to
  one has no business throwing away the other. A memo whose stamp is absent, unparsable or
  from another version, and a memo whose redb file is corrupt, is deleted and started again
  empty; neither is ever converted and neither fails the pass. A memo another handle already
  holds redb's lock on is neither, and is reported as `CatalogBusy` rather than unlinked from
  under its holder.

- `civitai.redb`, the lookup's memo: one row per sha256 a lookup pass has ever asked a listing
  service about, keyed by the 32 bytes of that hash and valued by when it was asked and what
  came back. An answer of "asked, and unknown" is a row too, so a hash the service does not
  know is asked about again only after `RETRY_UNKNOWN_DAYS`, 30. The catalog opens it on first
  use and holds it open for its own lifetime, because redb takes one exclusive lock per file;
  `lookup_candidates` asks it through one read transaction for a whole listing rather than
  opening one per artifact it considers.
- `civitai.version`, that memo's own format, written and read exactly as `digests.version` is
  and for the same reason: what a third party said about a file's bytes and the shape of a
  catalog record are unrelated facts. A memo of another version, or one whose redb file is
  corrupt, is deleted and started again empty; one another handle holds is reported as
  `CatalogBusy`. It reads 2 since the listing gained the `trainedWords` a version carries, so
  every hash a version 1 memo held is asked about once more.

Both memos grow monotonically, with no eviction, and neither is counted by `Catalog::storage`
or `hoard doctor`, which report the catalog directory alone. Deleting either by hand at any
time is safe: `digests.redb` costs the re-reads a later pass then pays, and `civitai.redb`
costs one request per hashed diffusion artifact the next time the lookup is switched on. A
`FORMAT_VERSION` rebuild re-infers every artifact from disk, after which the hashing pass
restores the digests from the one memo and the lookup restores its refinements from the other,
without reading a file or opening a socket.

On the fixture of `a_rebuilt_catalog_recalls_digests_the_memo_already_knows`, three files of
2,359,296 bytes in all: the first pass reports `HashProgress.bytes_done` of 2,359,296, and a
pass over a catalog scanned again from scratch into the same path reports 0 with
`HashReport.hashed` still 3 and every recalled digest byte-identical to the first pass's. A
file whose content and mtime changed between two passes is read for real again and gets the
digest of what it holds now, which is what
`a_changed_file_is_rehashed_despite_a_stale_memo_entry` pins, and a file whose
`lfs_threshold` answer moved is read again too, which is what
`a_threshold_change_rereads_a_file_whose_git_sha1_answer_moved` pins.

## How a record is encoded

native_model writes an eight byte header naming the record and its version, then a body in
whichever codec `#[native_model(with = ...)]` names. `catalog/codec.rs` holds two, and the
records that do not name one keep native_model's own bincode default.

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
- `Manifest` alone keeps native_model's default. It does not carry the format:
  `open_checked` reads the `FORMAT` stamp before redb is opened, so no record of a catalog
  written in another format, the manifest included, is ever decoded.

Hashes and row ids serialize as bytes and integers to a binary format and as hex and
prefixed strings to a human-readable one, so the catalog is compact and the JSON the API
answers with is unchanged. `hoard-core`'s `id.rs` is where that branch lives.

`Entry` carries no location text. The unique index over where a row sits is computed from
the three fields that say so, as `repo_id` big endian, then `revision` big endian, then the
relative path's own bytes. The first 8 bytes are exactly one repository and the first 12
exactly one revision, so a repository listing and a revision listing are both one prefix
scan of that index and `Entry` needs no repository index of its own.

`RevisionText` holds one revision's card and the directory that card sits in, keyed by the
same 12 bytes an entry's location starts with, so the card an artifact's documents are indexed
from is one point lookup by `(repo_id, revision)` and one repository's cards are an 8 byte
prefix scan. Every artifact of a revision reads that one row rather than carrying a copy; a
build reads it once per run of entries that share a revision. A document carries the card text
only when the card covers the entry it is written from, which `RevisionText::text_for` decides
from the stored directory: a `README.md` inside one folder does not describe the weights above
it or beside it. A card whose text or whose directory changes marks every artifact of its
revision for indexing, because nothing else about those artifacts moved.

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
- `scan/mod.rs` the pass itself: `scan_sources`, `rescan`, the walk loop, the summary, `CHUNK`.
- `scan/counters.rs` `Counters`, `DerivedShift`, the summary parts, `log_changes`, id allocation.
- `scan/walk.rs` the walk state and the unchanged-or-changed decision for a revision and a unit.
- `scan/plan.rs` discovery to units to prepared entries: shard sets, documents, roles, sniffing.
- `scan/store.rs` one entry, its artifact and its repository row written in one transaction.
- `scan/sweep.rs` the artifact sweep and every retirement of a revision, a source or a row.
- `scan/sources.rs` the source rows, their scanned counts and the configuration validation.
- `scan/revisions.rs` the revisions discovery found merged with the revisions the catalog holds.
- `naming.rs` the configs and cards a revision carries, each scoped to the directory it sits
  in, artifact names, per-file hashes.
- `search/mod.rs` `QueryRequest`, `QueryPage`, `RefreshReport` and what the catalog imports.
- `search/schema.rs` `Fields`, the keyword and column constants, the document one entry writes.
- `search/build.rs` the rebuild, the metered writer, per-entry documents, the directory publish.
- `search/refresh.rs` `refresh_search`, the change log and its cursor, the published directory.
- `search/answer.rs` a query from request to page: the path, the facet rail, the rows.
- `search/inspect.rs` the segment, delete and document counts the tests and diagnostics read.
- `search/paging.rs` the page window, the heap or sampled selection and its boundaries.
- `search/tally.rs` the facet tallies per segment and the base rail buckets.
- `search/collapse.rs` the per-artifact collapse, the spelled terms and the column blocks.
- `search/collectors.rs` the Tantivy collectors: primary, sampling, bucket, window, count.
- `query.rs` the grammar compiled against the Tantivy index.
- `read.rs` the inspector, the repository screens, the triage queue, the sources listing.
- `verdict.rs` a user's override of what an artifact is, re-applied after a rescan.
- `seed.rs` synthetic records for tests, behind `cfg(test)` or the `fixtures` feature.
- `userdata.rs` shelves, settings, the noise list, projections.
- `usage.rs` the served-response log and per-artifact use counts.
- `loadouts.rs` loadout documents and pin resolution.
- `jobs.rs` scan, apply and pull job rows.
- `hashing.rs` sha256 and git blob sha1 per file, content identities, merges.
- `hash_memo.rs` the `<data_dir>/digests.redb` sidecar: a digest against the inode, mtime and
  size that earned it, and its own version stamp.
- `lookup_memo.rs` the `<data_dir>/civitai.redb` sidecar: what a listing service answered
  about one sha256 and when it was asked, and its own version stamp.
- `lookup.rs` the artifacts a lookup pass may ask about and the writer that applies an answer:
  it refines the stored base, records one `lookup` evidence row whatever it decided, and moves
  the same counters, change log and search refresh a verdict does.
- `ingest.rs` a file the mirror fetched, filed as a served entry.
- `nodes.rs` this machine and every peer it knows.
- `digest.rs` the rows this node offers and the rows a peer advertises.
- `pulls.rs` a pull in flight and the file it lands.

## What the scan produces

- Existing HF-cache, Git and loose-source discovery feeds the store directly. Header sniffing
  and inference reuse the existing Rust implementations.
- A loose folder's one revision carries the sha its own contents fingerprint to: the sorted
  `(relative path, fingerprint, size)` triples of every entry whose bytes this disk holds,
  hashed to the 20 bytes a revision is named by. The Hub face, `x-repo-commit` and
  `hf download` therefore answer from the index rather than reaching huggingface.co. A
  synthetic revision is still matched across scans by name, so changing the folder's content
  moves the one revision's sha rather than accreting a second revision. Each such entry keeps
  the fingerprint the sha was derived from, so a rescan opens only the paths whose inode,
  mtime or size moved and folds the stored fingerprint in for every other: an unchanged loose
  folder costs no read at all, which is what the hourly watcher reconciliation does.
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
- A rule's name test is answered over every tensor name the header holds, never over the
  64-name sample. `sniff::summarize_tensors` fills `TensorSummary::markers` with the rule
  vocabulary it found while it still holds the whole key list: the fragments, the leading
  prefixes, and the exact keys a name equals once `model.diffusion_model.`,
  `diffusion_model.`, `model.` or `net.` is stripped. The vocabulary is derived from the rule
  tables themselves (`infer::rules::vocabulary`), so a rule cannot name a term the sniffer
  does not look for, and a `debug_assert` in `KeyMatch::matches` fails the rule's own test
  when it names one. One Aho-Corasick pass per header covers the fragments. `markers` is
  never stored (`#[serde(skip)]`): inference only ever runs straight after a fresh sniff.
  Samples are still 64 tensors, and the sampler guarantees a slot to the first tensor naming
  each key a rule reads a shape from, so `attn2.to_k` no longer loses its slot to `attn1.to_k`
  on a 2,500-tensor unet. Merged shards union their markers.
- A base is named by three tensor tiers and then refined. The pattern tier
  (`infer::rules::BASE_RULES`) is unchanged. The exact-key tier
  (`infer::families::FAMILY_RULES`) names a family by the keys ComfyUI's `model_detection.py`
  detects it by, and applies to every role but `vae`, whose latent spaces share their tensors;
  the two families that share `txt_norm.weight` part on the width it normalizes, and a third
  width names neither of them. The module tier (`ADAPTER_RULES`) names a LoRA by the modules
  it patches and is read only for `lora` and `ctrl`, because a host of another family carries
  the same module names. A lineage of a proven architecture (`hoard_core::Base::LINEAGES`) is
  then read from the file name or the one folder the file sits in, never from the rest of an
  absolute path, and stored under its own key, `Pony` as `pony`; that one table says it runs
  on `sdxl`, which is how `base:sdxl` still finds it. A lineage token never names an
  architecture on its own.
- Only weight and dataset formats become artifacts; every file is still an entry record.
- Shard sets, whether named `model-<n>-of-<m>` or listed by a `*.index.json` weight map,
  become one artifact anchored on the lowest shard. Members are entries pointing at that
  artifact with no search row; the anchor carries the whole model's size, shard count and
  `incomplete` flag, and an incomplete set is triage. A file whose safetensors or GGUF header
  parsed and reported zero tensors is not a model: it creates no artifact and anchors no set.
  A file whose header would not parse at all still becomes an artifact, so it reaches triage.
  A set is named `<directory>/<stem>.<extension>`, its own path inside the revision, and two
  sets that would read the same are told apart by how many files each holds.
- **A set is what declares two or more files, not what holds two.** A weight map naming at
  least two files is a set as soon as one of them is present and unclaimed, so one
  `model-00007-of-00014.safetensors` beside an index naming fifteen is that set with one
  member: the row is triage and the queue reads `1 shard present, incomplete`. The declared
  total is the count of distinct files the map names, never what a file name spells. A name or
  a map declaring one file declares no set, and `-00000-of-00001` is a single file.
- **A declared set claims its stem.** Where a weight map produced a set for one directory,
  stem and extension, the naming convention forms nothing further from the unclaimed files of
  that stem, whatever total their names spell: they go back to standing for themselves, and
  the zero-tensor rule keeps a placeholder from becoming an artifact. One set left is one set
  to name, so no `(2 files)` suffix is spoken over a second set that was never there. A
  disambiguating suffix counts in the plural it has: `(1 file)`, then `(2 files)`.
- A shard's own file name yields no trigger words: `infer_trigger`'s file-name fallback
  refuses a name `shard::parse_shard_name` reads, so `model-00001-of-00004.safetensors` has no
  trigger while `style-2-of-hearts.safetensors` keeps its words.
- A repository carries the tool layout loose discovery recognized, `Repository::tool_layout`,
  spelled as `ToolLayout::as_str` does. It is set exactly for a category folder of a
  recognized root: the layout was recognized, the unit's org is the source's own, **and** the
  layout is one whose top level folders are categories. ComfyUI and A1111 keep unrelated
  weights side by side in `loras` or `Stable-diffusion`, so every folder of one carries the
  layout and so does the bag of files at the root; a Kohya training root and a root holding a
  `.gguf` keep one model per folder, so only the root-files unit carries it there. A publisher
  folder inside such a root, a per-model folder of a plain loose source, an hf-cache
  repository, a git clone and every mirror, pull or peer repository carry nothing, and the
  server's category-folder test is that one field rather than a comparison of an org with a
  source name.
- A verdict cannot make a set complete, and a rejection ends everything.
  `ArtifactRecord::needs_triage` is the one predicate: the triage queue, the summary, the
  entry state a scan writes and a verdict settling an artifact's paths all read it, and it
  begins at the stored `rejected` flag. The summary counts it only for a record some local
  path holds (`path_count > 0`), so a row only a peer's digest created asks this node nothing.
  An incomplete record is queued whatever a verdict said, so confirming a role on one leaves
  it in the queue with the incomplete reason, and a verdict naming only a base, or one whose
  role is `unk`, leaves the artifact queued, counted and shelved alike: the queue, the
  summary's `triage_artifacts` and the `state:triage` shelf are one number after every
  verdict. `Catalog::record_verdict` is the one verdict path, used by `put_override`,
  `clear_triage` and `remove_noise`: it writes what the verdict says on the record, decides
  `needs_triage` once off the record it is about to store and, when the record no longer needs
  triage, moves every `triage` path of the artifact to `local` through `leave_triage`,
  `DerivedShift` and `log_changes` in the same transaction. A `Verdict::Restored`, which is
  what taking back a rejection is, does the mirror through `enter_triage`: every `local`
  path this node holds goes back to `triage` at once and a `served` one keeps its mark and
  takes the flag, exactly as `leave_triage` leaves it served when it clears the flag, so the
  three numbers rise together rather than waiting for a scan. A `Noise` row names the artifact it was rejected
  from, and `Catalog::remove_noise` restores that artifact when no other noise row still names
  it. Landing the missing files, a rejection, or a rescan dropping a path rejected as noise, is
  what takes an artifact out.
- A set record stores the total it declares. `ArtifactRecord::declared_total` is
  `ShardSet::declared_total` as the scan's grouping pass settled it, `None` for a single file
  and for a set nothing declared a total for; `SearchRow::shard_total` carries it to the API,
  and the triage reason reads `1 of 15 shards present, incomplete` where it has one. No
  writer but a scan fills it: the mirror's ingest and pull paths file a fetched file as its own
  record and never join a set's, so a scan is still the only thing that turns a set complete.
- A set's row carries its anchor's sha256. The record's `artifact.sha256` is written at scan
  time from the anchor's presence, which the hf-cache discoverer fills from the blob name and
  the loose discoverer leaves empty; `hashing.rs::rekey_artifact` writes it for a set whose
  record carries none once the digest of its anchor - the entry with the lowest id among the
  artifact's entries, which is the lowest-numbered shard `store_unit` stores first - arrives,
  keeping the identity the set was filed under, because a set is not identified by one file's
  content. A member's digest changes the record not at all.
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
  content; at equal content a naming that spells the repository beats one that spells a path
  inside it, and the smaller name settles what is left: a shard set outranks the file its
  anchor is, and two revisions naming one file differently agree whichever of them the walk
  reaches first. Whether a naming spelled its repository is recorded on the record as
  `named_by_repo` when the name is chosen, never recomputed against whichever repository a
  later pass happens to be walking, so the comparison is a property of the two records and
  cannot settle differently depending on which one the walk reaches first.
- Ids come from a persisted allocator; the generation increments per rescan; artifact path
  counts and copy counts are swept from the entry index after every scan and orphaned
  artifacts are removed. `path_count` is how many paths reach the artifact; `copies` is how
  many distinct inodes those paths amount to, so a file reached through a symlink is one
  copy and nothing about it is reclaimable. The `dupe` mark, the `deduped` flag and the
  summary's reclaimable bytes all read `copies`, never `path_count`.
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

`Catalog::apply_lookups` writes artifact records and takes the same path a
verdict does: one transaction for a batch, `touch` before the record is written, the artifacts
whose base moved pushed onto `pending` and logged, `settle_summary`, then one `refresh_search`
for the batch and only when something moved. It writes no entry, so it moves no derived count.
An answer that refines nothing still writes the artifact's cold `ArtifactDetail` row with one
`lookup` evidence, which is how the same hash is never asked about twice; that row is not in
the search document, so it is not logged and does not refresh the index.

A rescan recognises a stored row by where it is filed. `repository_stamps` keys the map it hands
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
fresh slot pays for it and only one repository's worth is held. The map of stored rows is
48 bytes a row and is loaded the same way, one repository at a time as the walk reaches it;
the rows that repository did not see are set aside when the walk leaves it, and the stored
repositories the walk never reached are streamed and retracted at the end of an uninterrupted
pass.

A pass that commits many times can also be asked to stop. `Catalog::rescan_until` checks a
`Stop` before each source it discovers and between the units of the one it is inside, and
`Catalog::hash_pending_until` checks it before the pass opens, before each batch and once per
megabyte of every file it reads, so a shutdown costs a megabyte rather than an 80 GB
checkpoint. What it cannot cut short is planning the source it is already inside or reading
the window of repositories it has reached, so an exit costs a batch, a unit, or one source's
plan and one window's discovery, never a whole pass. Both
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

`catalog/tests/recovery.rs` holds the proof. It interrupts a stage from the
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

A document writes the canonical name, entry-relative path, source and repository names, org,
base, trigger words and tensor keys into the word-prefix field (`Fields::text`, currently
`path_text_v1`). The revision's card, when it covers the entry, goes into `card`. Both fields are cut by
the `hoard_words` tokenizer, so one analyzer serves both sides of a query; they are separate
fields because a free-text word is matched from its start against the path/name field and whole against
`card`, which is what keeps `wan` from finding a card that says `want`.

One document per artifact carries a `primary` mark: the entry a row would be assembled from,
which is the highest availability preference and on a tie the lowest entry id. Queries
without entry-sensitive filters use that mark unless sorting on revision, node or state.
Name sorting alone can use the mark: its name column already describes the preferred copy's
display label. Free text uses matching entries instead because paths and aliases can differ.

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
node or state, still collapses. Free text also collapses: relative paths and filename aliases
can differ between copies. These fields differ between the entries of one
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
distinct artifact and node totals, and the six facet dimensions. Each dimension is counted
over the query with its own key excluded, so clicking a bucket never collapses the rail it
was clicked in. The `base` rail is at most 48 buckets, each counted by running its own query,
because the prefix rule that groups base labels cannot be answered from the tally. The
candidates are the most populous bases by exact term plus every family root among the rest
that one of them extends, since a root carried by few artifacts but prefixing many is a
larger bucket than its tally says; the whole candidate set is counted and then cut to 32 by
that count. Only a base the tally already holds is a candidate, so the cap drops buckets and
never invents one, and the candidate set is bounded, so the searches one query pays for the
rail do not grow with the number of bases the catalog holds. A collapsed query counts a
bucket by folding the artifact id alone, with no tallies and no row.

Name sorting uses the selected loose file's relative path, while repository models and shard
sets retain canonical labels. `SearchRow::display_name` and the search sort column follow
the same rule. The API carries canonical `name` separately from `display_name`, `relative_path`
and `entry_id`. `Catalog::artifact_row_at` validates entry ownership; explicit location-aware
reads and actions must not silently return another copy.

Free text indexes relative paths, filename aliases, sources, repositories, canonical names,
organizations, bases, triggers, tensor key prefixes and the body of
the shallowest README in each revision, into the entries that README's directory covers.
Tantivy uses one writer thread with a 32 MiB indexing budget; nothing larger than a unit is
held in memory or in an open transaction.

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

Run `search`, `status`, `rebuild` and `compact` one after another; the lock is exclusive. Use temporary catalogs for validation and inspect errors before retrying a failed scan.

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

## Query contract

The [Hoard API reference](api/hoard.md) owns the query grammar, filters, resource pagination
and facets. The web UI and CLI use that grammar. Use `hoard ls --help` and
`hoard catalog search --help` for the respective CLI defaults and options.

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
