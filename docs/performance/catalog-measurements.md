# Catalog measurements

These are historical measurements, anchored to the commits and workloads named in each
section. They are retained as performance evidence, not as current capacity guarantees or
an implementation backlog. Reproduce measurements on the target hardware before making
release or optimization decisions. In this report, "head" means the candidate at the time
of a measurement, never the current checkout. Results without a precise recorded revision
are contextual observations rather than reproducible release evidence. Current storage contracts are in
[the catalog reference](../reference/native-catalog.md); the
[benchmark task](../../tasks/benchmark-catalog/README.md) describes repeatable verification.

## Memory verification, 2026-09-08

Commit `51418ad` delivered **continuous hashing and metered index commits**. Its measurements
exclude the later follow-ups, grouped discovery, and usage changes. At that measured commit the broader redesign
was incomplete; these two improvements had attributable evidence and passed its measured gates.

The baseline is `bdf2964f954f2612c5a2a0ab4d33b9361aef8e00`. The selected production code
recovers hashing and indexing from `fef969a`, with comment cleanup and constant relocation.
The exact production patch, source/executable SHA256s, raw results, commands, and environment
are in [the compressed evidence archive](evidence/memory-2026-09-08.tar.gz).
Extract it with `tar -xzf docs/performance/evidence/memory-2026-09-08.tar.gz -C <scratch>`.
No temporary worktrees, synthetic catalogs, or benchmark executables are retained.

### Retained results

Two serial release runs per column, no `RUSTFLAGS`, ARM Cortex-X925/A725 workstation,
20 CPUs, rustc 1.95.0. Each cell gives run 1 / run 2. Catalogs are independently seeded
from the same restored synthetic files. All reported fixture tests actually executed.

| Targeted workload | Baseline `bdf2964` | Selected `fef969a` production, cleaned |
| --- | ---: | ---: |
| Mixed hashing cancellation checkpoint, ms | 574.22 / 570.07 | 9.40 / 9.36 |
| Four large-card artifacts: rebuild peak growth, MiB | 772.17 / 744.43 | 381.80 / 349.96 |
| Same rebuild, seconds | 6.483 / 6.634 | 6.575 / 6.506 |
| Same rebuild / refresh explicit commits | 1 / 1 | 4 / 4 |
| Same refresh peak growth, MiB | 394.77 / 738.85 | 353.08 / 224.62 |
| One oversized group: rebuild peak growth, MiB | 326.64 / 326.67 | 326.41 / 326.57 |

The hashing fixture puts a sparse 1 GiB file before 255 small files, cancels after at least
64 completions, resumes, and compares all 256 final hashes with `hash=now`. Continuous
workers remove the 64-file completion barrier. Work dispatch holds at most one queued file
per worker plus one 64-entry discovery page; the digest channel holds 256 results and the
writer at most 64. `[index] hash_threads` defaults to zero, selecting the Rayon pool size;
both automatic and explicit counts are clamped to 1–256. Full queues block producers.
Cancellation is checked between files and per MiB read; queued writes and bookkeeping must
finish before return. Existing configuration files remain valid.

The fan-out fixture gives each artifact 50–100 file entries and a roughly 4 MiB card.
Rebuild memory falls by about half, well beyond run spread, without a clear rebuild-time
change on this shape. Refresh memory varies substantially: do not infer a stable percentage
improvement from those two samples. Collapse answers, usage-linked refresh, and deletion-only
refresh are asserted. Index commits occur between artifact groups at an estimated 128 MiB;
**this is not a strict memory limit**. One group is constructed before submission and can
exceed the threshold, as the single-group row demonstrates. Existing rebuild/refresh APIs
remain; reporting APIs add explicit commit and document counts.

### Final scale gates

| 100k entries | Baseline `bdf2964` | Selected `fef969a` production, cleaned |
| --- | ---: | ---: |
| Scan, seconds | 4.998 / 5.083 | 5.005 / 5.050 |
| Scan peak growth, MiB | 167.91 / 167.66 | 167.80 / 168.53 |
| Rebuild, seconds | 0.660 / 0.667 | 0.705 / 0.681 |
| Allocated disk, bytes | 142635008 / 142630912 | 142651392 / 142643200 |
| Allocated disk, MiB | 136.03 / 136.02 | 136.04 / 136.04 |
| Unchanged rescan, seconds | 0.378 / 0.393 | 0.380 / 0.389 |
| Slowest of eight midpoint queries, ms | 13.96 / 13.98 | 14.69 / 14.78 |

| 1M entries | Baseline `bdf2964` | Selected `fef969a` production, cleaned |
| --- | ---: | ---: |
| Scan, seconds | 74.523 / 73.271 | 75.381 / 73.130 |
| Scan peak growth, MiB | 778.78 / 776.48 | 776.68 / 775.87 |
| Rebuild, seconds | 7.009 / 8.207 | 8.492 / 8.280 |
| Rebuild peak growth, MiB | 79.18 / 78.48 | 71.97 / 71.59 |
| Allocated disk, bytes | 1397100544 / 1396887552 | 1396789248 / 1396641792 |
| Allocated disk, MiB | 1332.38 / 1332.18 | 1332.08 / 1331.94 |
| Unchanged rescan, seconds | 4.288 / 4.493 | 4.567 / 4.415 |
| Rescan after 100k deletions, seconds | 16.161 / 16.879 | 16.014 / 16.295 |
| Slowest of eight midpoint queries, ms | 95.22 / 93.58 | 99.20 / 98.50 |

All gates pass: scan growth below 2 GB, allocated disk below 1,500,000,000 bytes, rebuild
faster than scan, and every search below 500 ms. The selected column's slowest individual
search, including collapse and post-refresh queries, is 114.31 ms. Metering adds rebuild
work on small records; the table reports that cost rather than claiming a speedup. Scan,
deletion, and disk differences are not attributed improvements. Raw logs include phase
memory, apparent disk, facets, counts, changed rescans, and usage costs.

### Concurrent latency diagnosis

The 500 ms gate applies to the **slowest of eight simultaneous midpoint requests**. Both
fixtures use a start barrier, page size 100, name ordering, midpoint 499950, and assert each
answer against a reference. HTTP includes the complete response body. Watchers are disabled;
controlled activity is a full rebuild started through the same barrier. Query-only checks
reopen the same intact synthetic format-7 probe catalog, independently of the deletion runs.

| Slowest request, ms | Baseline `bdf2964` | Selected `fef969a` production, cleaned |
| --- | ---: | ---: |
| Direct, idle | 99.42 / 91.06 | 93.39 / 96.60 |
| Direct, background rebuild | 93.73 / 101.23 | 93.25 / 94.13 |
| HTTP, idle | 100.78 / 106.52 | 102.28 / 99.81 |
| HTTP, background rebuild | 104.07 / 99.76 | 102.26 / 102.93 |

The historical 632–681 ms failure did not reproduce, including earlier fresh baseline
repeats. Separate diagnostic HTTP runs attribute 71.127–99.520 ms to collection;
blocking-pool waiting is 0.091–0.504 ms, shelves 0.011–0.033 ms, compile context
0.002–0.008 ms, index acquisition 0.003–0.010 ms, query compilation 0.007–0.017 ms,
facets 0.007–0.015 ms, page selection 0.836–0.870 ms, native reads 0.306–0.447 ms,
assembly 0.050–0.107 ms, and serialization 0.050–0.115 ms. Diagnostic serialization is
an extra serialization and is excluded from acceptance. No locking or scheduling change
is justified. Raw logs record each latency, wall time, process CPU, and RSS; process totals
include startup/reference queries and, where enabled, completion of the background rebuild.

### Compatibility, validation, and excluded changes

Catalog format stays **7**, with unchanged stored models and usage APIs. No migration,
reset, event dropping, or overload-policy change is introduced. The selected CLI opened a
baseline-created catalog after 2,000 served file responses and its `used:<1d` query, sorted
by use, returned the one served artifact. Existing tests cover shared-remote clones,
cross-revision refiling, hash merges, interrupted transactions, publication recovery,
and exact `used:` filtering/sorting. New tests exercise blocked-file progress, worker
limits, metered commits, collapse groups, deletion-only refresh, and interrupted replay.

Final validation: **888 tests passed**, nine intentionally ignored fixture/helper tests;
required ignored measurement fixtures were separately executed. Workspace build, Clippy
with warnings denied, and formatting passed. Actual CLI scans/searches and timeout-bound
servers passed all eleven HTTP routes, three times each, using 100k loose files plus a git
source and 2,000 served responses. Route medians are smoke evidence, not attributed gains:

| Route, ms | Baseline `bdf2964` | Selected `fef969a` production, cleaned |
| --- | ---: | ---: |
| Repositories, limit 1 / 100 | 0.463 / 0.775 | 0.311 / 0.361 |
| Models list / git model detail | 0.409 / 0.161 | 0.259 / 0.277 |
| HEAD git resolve | 0.483 | 0.479 |
| Sources | 0.305 | 0.452 |
| Search empty / node:self / role:lora | 3.371 / 6.293 / 3.434 | 3.568 / 6.244 / 3.798 |
| Triage | 1.911 | 2.191 |
| Populated repository usage | 0.774 | 1.063 |

The full recovery (`04adbd767d8cc9cbfbcf45ef1cf048bb168cba17`, peeled recovery tag) was
also measured twice. It reduced 1M scan growth from 775–779 MiB to 582–583 MiB, but did
not isolate summary folding from grouped discovery/cache changes. Those changes are removed
from delivery. It reduced retirement of 500k usage records from 38–40 s to 1.46–1.48 s,
but its format-8 recovery deletes and rescans incompatible catalogs, without demonstrated
preservation of history, overrides, shelves, and pins. Usage rekeying, reindex coalescing,
and writer changes are deferred; the linked-event fixture produced 1,954 refresh changes
in both columns and does not establish a coalescing benefit. Delayed-writer/backpressure
experiments were not completed, so no usage implementation is retained on their basis.
Source-wide retirement bookkeeping and usage-batch memory were unresolved at that measured
revision. This historical result is not a statement of the current implementation or backlog.
No user's catalog was opened by any experiment. Lost results from the first interrupted
session are excluded. Temporary checkouts and generated data have been removed.


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

Waves 1 to 7 of `docs/performance/catalog-measurements.md` changed the query
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
the number and takes no view on the mechanism, which no measurement here establishes. The
curve in the section below settles it at `df0b103`: the same burst on the same tree measures
107.990 and 102.730 ms, it is flat against the thread count at both scales, and it is flat to
within two per cent when the burst is pinned to one class of this machine's cores. The 632.5
and 681.3 ms belong to that run, not to the catalog.

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

### Query latency against concurrent thread count, at 100k and at 1M

The row above says the slowest of eight simultaneous midpoint queries took 632.5 and 681.3 ms
at `0935fde`, which is the only gate this document has ever recorded as missed. That number is
a single point: it cannot tell a lock from a bandwidth wall from a bad run. The harness now
takes the whole curve. `HOARD_SCALE_CONCURRENT_THREADS` is a comma-separated list of thread
counts, one burst per count against one already-seeded catalog, and `HOARD_SCALE_CONCURRENT`
keeps its old meaning as the enable flag, so a run without the new variable prints exactly the
rows it printed before.

```sh
HOARD_SCALE_ENTRIES=1000000 HOARD_SCALE_DIR=<scratch>/scale \
  HOARD_SCALE_CONCURRENT=1 HOARD_SCALE_CONCURRENT_THREADS=1,2,4,8 \
  cargo test --release -p hoard-index --test scale concurrent_catalog_queries \
  -- --nocapture --ignored
```

Everything below was measured at `df0b103` on this workstation, one session, nothing else
running, no `RUSTFLAGS`, release. Each scale was seeded once by `native_catalog_at_scale` and
every count ran against that same catalog, so no column pays for a different index. The page
is 100 rows at the midpoint offset, name order, each answer asserted against a reference page
taken before the burst.

| threads, 100k | slowest, ms | median, ms | burst wall, ms | peak RSS growth |
| --- | ---: | ---: | ---: | ---: |
| 1 | 14.296 / 14.232 | 14.296 / 14.232 | 15.697 / 14.409 | 438,272 B / 438,272 B |
| 2 | 14.601 / 14.634 | 14.470 / 14.490 | 14.938 / 14.798 | 376,832 B / 376,832 B |
| 4 | 14.761 / 14.744 | 14.514 / 14.464 | 15.055 / 15.027 | 753,664 B / 749,568 B |
| 8 | 14.445 / 14.377 | 14.287 / 11.502 | 14.842 / 14.670 | 1,507,328 B / 1,527,808 B |

| threads, 1M | slowest, ms | median, ms | burst wall, ms | peak RSS growth |
| --- | ---: | ---: | ---: | ---: |
| 1 | 91.415 / 90.025 | 91.415 / 90.025 | 92.446 / 91.131 | 856,064 B / 856,064 B |
| 2 | 76.117 / 91.237 | 76.074 / 91.220 | 76.676 / 91.680 | 798,720 B / 798,720 B |
| 4 | 90.873 / 89.317 | 76.024 / 77.121 | 91.401 / 89.946 | 1,581,056 B / 1,589,248 B |
| 8 | 107.990 / 102.730 | 77.288 / 77.131 | 108.545 / 103.474 | 3,174,400 B / 3,170,304 B |

Each cell gives run 1 / run 2. The 1M set was run twice because the 632 and 681 ms figures
differed by 8 per cent and one run cannot tell a curve from noise.

**The curve is flat at both scales, and the 632 ms figure did not reproduce in any vehicle.**
The slowest query at eight threads over the slowest at one is 1.01 and 1.01 at 100k, and 1.18
and 1.14 at 1M. Serialization would have put that ratio near eight; it is not near eight at
either scale, and the 1M ratio is inside the spread of the one-thread row itself, which
measured 76.011, 90.025 and 91.415 ms across three runs, a spread of 1.20. Aggregate memory is
bounded as it was: eight queries at 1M grow the process by 3.03 MiB, about four times what one
grows it by, and the median query at eight threads is *faster* than the single query in both
1M runs.

Three further runs rule out the readings that a flat curve can hide.

- **Reversed order**, `HOARD_SCALE_CONCURRENT_THREADS=8,4,2,1` at 1M, to separate the curve
  from warming: 98.124 ms at eight, 102.440 at four, 92.496 at two, 76.011 at one. Four threads
  slower than eight is not a curve; it is the band the individual queries sit in.
- **The vehicle the 632 ms came from**, `native_catalog_at_scale` at 1M with
  `HOARD_SCALE_CONCURRENT=1` and no thread list, so the burst runs where it ran then, right
  after the republished rebuild: the slowest of eight took 97.475 ms against 76.338 ms for the
  same page single-threaded in the same run, a ratio of 1.28.
- **Pinned to one core class.** This workstation is heterogeneous: ten Cortex-X925 at
  3.90 GHz (CPUs 5-9 and 15-19) and ten Cortex-A725 at 2.81 GHz (CPUs 0-4 and 10-14). Every
  unpinned burst above is bimodal, 76-78 ms or 89-108 ms at 1M and 8.4-9.1 ms or 14.2-14.9 ms
  at 100k, and which mode a query lands in does not follow the thread count. Under `taskset`
  the modes separate exactly and the curve goes dead flat:

| pinned burst | 1 thread, ms | 8 threads, slowest, ms | 8 threads, median, ms | ratio |
| --- | ---: | ---: | ---: | ---: |
| 1M, ten X925 cores | 76.337 | 77.760 | 77.644 | 1.02 |
| 1M, ten A725 cores | 127.891 | 129.686 | 128.947 | 1.01 |
| 100k, ten X925 cores | 8.480 | 9.066 | 9.019 | 1.07 |
| 100k, ten A725 cores | 14.288 | 14.874 | 14.839 | 1.04 |

Eight simultaneous midpoint queries cost one query's time plus one to two per cent, on either
core class, at either scale. The whole of the unpinned spread is the scheduler choosing a core
class, and the 1.68 between the two classes at 1M is close to the 1.39 between their clocks
with the rest in the wider core. Nothing in the query path serializes and nothing at 1M runs
out of memory bandwidth at eight readers: the 3.03 MiB the burst adds is the working set of
eight page selections, not eight copies of an index.

The 500 ms gate is met at eight threads with a margin of 392 ms on the slowest reading above,
and met on the slowest A725 reading, 129.686 ms, by 370 ms. The 632.5 and 681.3 ms of
`0935fde` stand as history from a run this document cannot re-enter; every reading taken since,
here and in the concurrent latency diagnosis above, is between 76 and 130 ms.

### What a rescan alone holds, at 100k and at 1M

The rescan rows above are differences of a high-water mark inside a process that has already
scanned, rebuilt and queried, so they measure where the mark was left rather than what the
rescan allocates: the same unchanged rescan reads 57 MiB in one run and 140 MiB in another.
Two standalone vehicles answer it instead, each an ignored test in
`crates/hoard-index/tests/scale.rs` that opens what an earlier `native_catalog_at_scale` left
under `HOARD_SCALE_DIR` and does one thing: `rescan_only_catalog` rescans the seeded catalog,
and `discovery_only_source` runs `discover::discover` over the seeded tree and holds the
result. The figure each reports is the process's absolute `VmHWM`, not a growth.

Measured at `b10c30d`, release, nothing else running, pinned to the ten Cortex-X925 cores with
`taskset -c 5-9,15-19`, each scale seeded once and every run against that seed:

| vehicle | scale | run | wall | peak, absolute | resident after |
| --- | --- | ---: | ---: | ---: | ---: |
| rescan only | 100k | 1 | 461.6 ms | 143.52 MiB | 123.08 MiB |
| rescan only | 100k | 2 | 355.5 ms | 140.48 MiB | 108.97 MiB |
| rescan only | 1M | 1 | 4.285 s | 866.25 MiB | 514.30 MiB |
| rescan only | 1M | 2 | 3.904 s | 870.54 MiB | 506.58 MiB |
| discovery only | 1M | 1 | 1.115 s | 460.33 MiB | 354.69 MiB held |
| discovery only | 1M | 2 | 1.159 s | 455.45 MiB | 349.65 MiB held |

Each process starts at 4 to 8 MiB. The first rescan of each scale found the 1,000 headers the
seeding run left rewritten and the second found nothing changed; the peak does not depend on
it, because an unchanged rescan builds the discovered source and the known stamps in full.

**The 1M peak is accounted for, within four per cent, by three things:**

| component | why it is resident at the peak | MiB |
| --- | --- | ---: |
| redb read cache | `known_stamps` reads every `Entry` before the walk; the catalog caps the cache at 256 MiB, of which redb gives nine tenths to reads | up to 230 |
| `walk.known` and `walk.seen` | a `HashMap<u128, Known>` of 64-byte pairs at 2^21 buckets, and a set of every entry id | about 147 |
| discovery | the source's `DiscoveredEntry` rows, 144 bytes each with two heap paths, plus the walker's single `Vec<DiscoveredFile>` for the whole source, still allocated while the rows are built from it | about 452 |
| baseline | the process before either | 8 |
| **sum** | | **about 837** |

against 866 to 871 MiB measured. Discovery's own peak is 105 MiB above what it holds when it
returns, which is the walker's buffer; `known` is alive during discovery because
`known_stamps` runs first, so the two add. The 6.1 ratio between the 1M and 100k peaks,
rather than ten, is the cache: it is bounded, so at 100k it is a larger share.

The prediction this was taken to test said roughly 390 to 470 MB and was wrong by about half.
It had the ranking right and the total wrong: it left out the redb cache, it left out
discovery's transient buffer, and it put the held discovered source at 254 MB where the
scratchpad's long absolute paths make it 350 MiB.

### The rescan peak after windowed discovery, at 1M

A scan or rescan now plans a source's repositories, then reads, stores and walks them one
window at a time, so neither the walker's buffer nor the discovered rows outlive the window
being walked. The harness's tree is one repository of a million files, which a window of
repositories cannot bound, so the change is measured over the same tree filed one repository
per folder: `HOARD_SCALE_PER_FOLDER=1` makes `config` in `crates/hoard-index/tests/scale.rs`
set `repo_per_folder: true`, a thousand repositories of 1,000 files, seeded into
`native-1000000-per-folder` beside the one-repository seed. Unset, `config` is what it was at
`b10c30d`.

Both binaries were built before the first timed run, release, nothing else running, pinned to
the ten Cortex-X925 cores with `taskset -c 5-9,15-19`. The before binary is `b10c30d` with only
the harness knob added; the after binary is the wave commit "Discover and store one repository window at a time", which
carries the same knob. The per-folder tree was seeded once by the before binary
and every rescan below ran against that seed:

| tree | binary | run | rescan wall | peak, absolute | resident after |
| --- | --- | ---: | ---: | ---: | ---: |
| per folder | `b10c30d` | 1 | 4.291 s | 821.61 MiB | 613.82 MiB |
| per folder | `b10c30d` | 2 | 4.220 s | 832.78 MiB | 470.38 MiB |
| per folder | branch | 1 | 4.222 s | 477.62 MiB | 277.09 MiB |
| per folder | branch | 2 | 4.256 s | 477.53 MiB | 280.83 MiB |
| one repository | `b10c30d` | 1 | 4.257 s | 875.59 MiB | 523.51 MiB |
| one repository | branch | 1 | 3.858 s | 866.50 MiB | 511.64 MiB |

The first run of each tree found the 1,000 headers the seeding run left rewritten and every
later run found nothing changed.

**The per-folder rescan peak is met: 477.6 MiB against a threshold of 600, from 821.6 and
832.8.** It is 57 MiB above the 420 the plan estimated, and this run does not attribute that
difference. The rescan wall moved by less than the spread of its own runs: 4.239 s after
against 4.256 s before on average, a ratio of 1.00, so the planning walk a loose per-folder
source now pays before its windows are read does not show at this scale. What is resident
after the rescan fell from 470 and 614 MiB before to 277 and 281 MiB after.

**The one-repository tree does not move, and that is the design limit.** A window is made of
repositories, and shard sets, a revision's documents and the unchanged-revision test all span a
revision, so a repository of a million files is read and held whole. Its 866.5 MiB against
875.6 MiB is within the spread of the earlier two runs at `b10c30d`, and its wall is not
comparable because only the before run found changed headers.

The seeding run, once per binary over the per-folder tree, with the same pinning:

| row | `b10c30d` | branch |
| --- | ---: | ---: |
| scan wall | 80.503 s | 77.101 s |
| peak rss growth during scan | 629.65 MiB | 324.71 MiB |
| rebuild wall | 9.275 s | 9.122 s |
| unchanged rescan wall | 4.088 s | 4.016 s |
| unchanged rescan peak rss growth | 410.21 MiB | 194.56 MiB |

A fresh scan reaches the same walk, so its peak growth halves with the rescan's, and its wall
changes by a ratio of 0.96. That is one run a binary, so it is not a result beyond saying the
scan did not get slower. The rebuild does not touch discovery, and its row is variance.

### The rescan peak after per-repository stamps, at 1M

A rescan now loads a repository's stamps, revision sizes and seen set when the walk reaches
it. When the walk leaves that repository, the rows it did not see move to a pass-wide list.
At the end of an uninterrupted pass that list is retracted, and so is every stored repository
of the walked sources that the walk never reached, streamed from its prefix in `CHUNK`
transactions. An interrupted rescan still retires nothing.

Both binaries were built before the first timed run: release, nothing else running, pinned to
the ten Cortex-X925 cores with `taskset -c 5-9,15-19`. The before binary is `9dd3eb8`. The
after binary is the wave commit "Hold one repository's stamps at a time". Each seed opened with
1,000,000 present artifacts, and every run below found nothing changed but the first after run
on the one-repository tree, which found the 1,000 headers a reseed of the shared tree had
rewritten; its peak is in line with the second. The two binaries were run back to back on each
seed in the order shown.

| tree | binary | run | rescan wall | peak, absolute |
| --- | --- | ---: | ---: | ---: |
| one repository | `9dd3eb8` | 1 | 3.899 s | 862.51 MiB |
| one repository | `9dd3eb8` | 2 | 3.773 s | 860.63 MiB |
| one repository | wave commit | 1 | 3.869 s | 846.48 MiB |
| one repository | wave commit | 2 | 3.544 s | 848.27 MiB |
| per folder | `9dd3eb8` | 1 | 4.179 s | 477.55 MiB |
| per folder | `9dd3eb8` | 2 | 4.205 s | 477.50 MiB |
| per folder | wave commit | 1 | 3.518 s | 271.84 MiB |
| per folder | wave commit | 2 | 3.490 s | 272.33 MiB |

**The gate is met: the per-folder rescan peak fell by 205 MiB, from 477.5 to 272.1 MiB, against
a required fall of 100 MiB to 378 MiB or below.** Each binary's two runs agree to within
0.5 MiB. The per-folder rescan wall fell from 4.19 s to 3.50 s, a ratio of 0.84, larger than
either pair's spread.

**The one-repository tree fell by 14 MiB, to 847.4 MiB**, where the plan expected it not to move.
It did not get there first time. The wave as first implemented rose to 944.91 and 940.20 MiB on
this tree against 861.83 and 866.07 at `9dd3eb8`, because a repository's stamps now load after its
window is discovered: for a repository of a million files, the stamp map growing by doubling,
with the half-size table alive through each resize, and the per-artifact `shards` cache sat on top
of the discovered rows, where at `9dd3eb8` both were built and dropped before discovery began.
Two changes removed it, each measured on the same seed. Sizing the stamp map from the
repository's stored `present_files` before its scan brought the peak to 886.95 and 896.48 MiB.
Caching in `shard_count_of` only the artifacts that have a shard count, which are the only ones
a cache spares a repeated read of a shard set's members, brought it to the figures in the table:
the cache no longer holds an entry per artifact of an unsharded repository. The design limit is
unchanged: a repository is read and held whole.

The per-folder fall is 58 MiB more than the 330 MiB estimate, which removed only the 147 MiB of
`walk.known` and `walk.seen`. The pass-wide `shards` map `known_stamps` held, an entry for every
artifact of every source, is gone as well; by arithmetic from its type sizes it came to about
51 MiB with its resize table. That is arithmetic, not a separate measurement.

What the per-folder peak holds now is up to 230 MiB of redb read cache, filled by a pass that
reads every entry, the 8 MiB baseline, and about 34 MiB for one window of discovered
repositories and one repository's stamps.

The full harness, `native_catalog_at_scale` with `HOARD_SCALE_PER_FOLDER=1
HOARD_SCALE_DELETE=1`, ran once per binary with the same pinning, `9dd3eb8` first, before the two
changes above; neither changes a scan, a rebuild or a retraction, so its rows stand. Each run
seeded the per-folder catalog again:

| row | `9dd3eb8` | wave commit |
| --- | ---: | ---: |
| scan wall | 78.431 s | 78.568 s |
| peak rss growth during scan | 325.11 MiB | 323.69 MiB |
| rebuild wall | 9.110 s | 9.198 s |
| rescan wall, 1,000 changed | 4.310 s | 3.703 s |
| unchanged rescan wall | 4.226 s | 3.501 s |
| unchanged rescan peak rss growth | 194.40 MiB | 0.81 MiB |
| deletion rescan wall, 100,000 files removed | 16.506 s | 12.795 s |
| peak rss growth during the deletion rescan | 194.56 MiB | 0.09 MiB |
| deletion rescan report | 900,000 unchanged, 100,000 absent | 900,000 unchanged, 100,000 absent |

A fresh scan now seeks each repository's empty stamp prefix. Its wall moved by a ratio of 1.00
and its peak by 1.4 MiB, so that seek does not show. **The two rescan growth rows are not
comparable as peaks.** Each is growth over the high-water mark the scan left in the same
process. After this wave neither rescan exceeds that mark, so the rows read near zero, and the
rescan-only runs above are the absolute measure.

The deletion rescan is where the new pass-wide `missing` list grows: 100,000 rows at 48 bytes,
about 4.6 MiB by arithmetic. It did not raise that process's high-water mark. Its wall fell by
a ratio of 0.78, but that is one run per binary, so it only shows the list did not slow the
retraction. The rebuild row is variance, since this wave does not touch it.

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
45.4 per cent. `docs/performance/catalog-measurements.md` reads redb 2.6.3 and
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

### A scan of this workstation's real caches

Measured at `568ec5f` on the workstation this repository lives on: ten Cortex-X925 at
3.90 GHz (cpus 5-9, 15-19) and ten Cortex-A725 at 2.81 GHz (cpus 0-4, 10-14). Every timed
command below is pinned to the X925 set with `taskset -c 5-9,15-19` and read with
`/usr/bin/time -v`. `cargo build --release` finished before the first timed command and
nothing else ran beside one. The scratch config set `[mirror] offline = true` and
`hash_policy = "never"`; nothing under the three sources or under `<checkout>`
was written.

**What the three sources hold.** `du -sb` over each root, apparent size, not the catalog's
count:

| source | path | `du -sb` | files (`find -type f`) |
| --- | --- | ---: | ---: |
| hf-cache | `~/.cache/huggingface/hub` | 345,344,151,695 bytes (321.6 GiB) | 1,085 |
| comfy | `<home>/ComfyUI/models` | 893,979,929,241 bytes (832.6 GiB) | 344 |
| lmstudio | `<home>/.lmstudio/models` | 405,827,252,128 bytes (378.0 GiB) | 28 |

1.5 TB total, 1,457 files. This is the opposite shape from the synthetic tree: the synthetic
tree is 100,000 to 1,000,000 files of one tensor each, and this real tree is a little over a
thousand files, most of them tens of gigabytes each, filed under 103 repositories. Nothing
below is comparable to the 100k or 1M rows elsewhere in this document, and it says so at each
table rather than only here.

**The scan**, `hoard --config <scratch>/hoard.toml scan --hash never` against an empty data
directory, so this is `Catalog::open_or_scan`'s build followed by the CLI's own explicit
rescan (see "Defects" below, item 1):

| measure | value |
| --- | --- |
| wall | 3.35 s |
| peak RSS (`/usr/bin/time -v`) | 337,828 KiB (329.9 MiB) |
| exit status | 0 |
| warnings or errors logged | none |
| report | `unchanged=733 changed=0 added=0 absent=0` (see item 1: this is the CLI's second pass, not the build) |
| sources / repositories / artifacts / entries (`hoard catalog status`) | 3 / 103 / 235 / 733 |
| `du -sb` of the data directory right after the scan | 5,786,908 bytes (5.52 MiB) |

**Two unchanged rescans**, each `hoard --config <scratch>/hoard.toml scan --hash never`
against the now-populated catalog, nothing on disk touched between them:

| run | wall | peak RSS | report |
| --- | ---: | ---: | --- |
| rescan 1 | 0.04 s | 16,308 KiB (15.93 MiB) | `unchanged=733 changed=0 added=0 absent=0` |
| rescan 2 | 0.05 s | 15,664 KiB (15.30 MiB) | `unchanged=733 changed=0 added=0 absent=0` |

Both rescans report zero changed, added and absent, which is correct for a source nothing
touched.

**`hoard doctor`** exits 0, every line `ok`: git, the data directory, catalog format 7, the
node, 0 known peers, the summary (235 artifacts, 235 present, 103 repositories), the search
index (built, 0 pending changes), the three sources (hf-cache 477 files, comfy 228 files,
lmstudio 28 files), the mirror cache (not created yet, offline), and upstream (offline mode,
the network is never touched).

**The routes**, three runs each against real repositories read back from `GET
/api/v1/repos`: `models/Qwen/Qwen3-Coder-Next` (6 files, 1.7 MB, hf-cache) for the model,
detail, resolve and usage rows, and the term `qwen` for the added free-text search row. Server
and requests ran as one `timeout`-wrapped foreground command; `pgrep -af 'target/release/hoard
serve'` found nothing once it returned.

| route | status | median |
| --- | --- | ---: |
| `GET /api/v1/repos?limit=1` | 200 | 0.617 ms |
| `GET /api/v1/repos?limit=100` | 200 | 0.607 ms |
| `GET /api/models?limit=1` | 200 | 0.454 ms |
| `GET /api/models/Qwen/Qwen3-Coder-Next` | 200 | 0.549 ms |
| `HEAD /Qwen/Qwen3-Coder-Next/resolve/main/config.json` | 200 | 0.522 ms |
| `GET /api/v1/sources` | 200 | 0.358 ms |
| `GET /api/v1/search?q=` | 200 | 1.307 ms |
| `GET /api/v1/search?q=node:self` | 200 | 1.274 ms |
| `GET /api/v1/search?q=role:lora` | 200 | 0.709 ms |
| `GET /api/v1/triage?limit=20` | 200 | 0.654 ms |
| `GET /api/v1/repos/models/Qwen/Qwen3-Coder-Next/usage` | 200 | 0.234 ms |
| `GET /` | 200 | 0.205 ms |
| `GET /api/v1/search?q=qwen` (added; a real free-text term) | 200 | 0.510 ms |

Every route answered 200 on every run. All thirteen rows sit under 1.5 ms, which says nothing
about the 1M gate of 500 ms: a 733-entry catalog exercises none of the paging, tallying or
collapse cost that gate is for.

**`hf download`** of one small file, `config.json` (1,178 bytes) from
`Qwen/Qwen3-Coder-Next`, through `HF_ENDPOINT=http://127.0.0.1:7399` with `HF_HOME` under the
scratch directory: exit 0, the downloaded blob is byte-identical to the source file in
`~/.cache/huggingface/hub`.

**Defects.**

1. **`hoard scan` on an empty data directory scans twice, and reports the second pass as
   "unchanged."** `src/cli/node.rs::open_catalog` calls `Catalog::open_or_scan`, which is the
   real first build and is what logs the `"building the native catalog"` phase lines above.
   `src/cli/scan.rs::run` then unconditionally calls `catalog.rescan(...)` again over the same
   sources, and that second pass is what the `"scan complete"` line reports. On this run the
   second pass cost 0.04 s against the 3.35 s of the command as a whole, because these
   sources hold 733 files; the ratio would not stay negligible on a source the synthetic
   harness's 1M-entry tree is sized like. The harness itself does not have this defect:
   `native_catalog_at_scale` in `crates/hoard-index/tests/scale.rs` times
   `Catalog::scan_sources` alone for its "scan wall" row and times `catalog.rescan` separately
   for its rescan rows, so the "scan wall" figures elsewhere in this document are the build
   alone, not what the CLI's `hoard scan` actually costs on an empty directory. A first-time
   `hoard scan` is more expensive, and reports itself as "unchanged," which is the wrong word
   for a catalog that a moment ago had nothing in it.
2. **A source's reported byte total overcounts a repository that keeps more than one
   revision.** `GET /api/v1/sources` and `hoard doctor`'s `source` line both read
   `SourceRecord.byte_count`. A scan builds it by summing `entry.size` over every present
   entry, one entry per filed path: `DerivedShift::shift` in
   `crates/hoard-index/src/catalog/scan/counters.rs` adds each entry's size to its repository's
   `present_bytes`, and `held_by_source` in `crates/hoard-index/src/catalog/scan/sources.rs`
   adds those per source when `record_scan_stats` stores the source row. (`seed.rs` repeats the
   same per-path sum for test fixtures only; it is compiled under `cfg(any(test, feature =
   "fixtures"))` and is not what a real scan runs.) An hf-cache revision's
   files are symlinks into `blobs/`, named by hash, so a file unchanged between two snapshots
   of the same repository is one blob on disk and two entries in the catalog, each carrying
   the blob's full size. On this workstation two hf-cache repositories keep two snapshot
   revisions each
   (`models--mistralai--Ministral-3-14B-Instruct-2512`,
   `models--nvidia--NVIDIA-Nemotron-3-Nano-30B-A3B-BF16`), and the hf-cache source reports
   414,409,479,660 bytes where the source's own `blobs/` directories sum to 335,552,746,441
   bytes (`du -sb` over each repository's `blobs`) and the whole `hf-cache` directory to
   345,344,151,695 bytes: the reported total is 78,856,733,219 bytes (73.4 GiB), 23.5 per
   cent, above what is actually on disk. The Hub routes this total is not used by are correct
   regardless - `resolve` and `hf download` above read the entry itself, not this aggregate -
   but the number `doctor` and the sources panel show next to a source's path is not that
   source's disk footprint whenever a repository's revisions share content.

No other defect was observed: every command exited 0, every route answered 200, both rescans
reported zero changed, and the downloaded file matched its source byte for byte.
