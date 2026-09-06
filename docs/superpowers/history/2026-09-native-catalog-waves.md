# Native catalog: the wave-by-wave record

History, condensed out of `docs/native-catalog.md` so that document could describe the
catalog as it is rather than how it was built. Nothing here is current. For the current
state, the current numbers and what is left to do, read `docs/native-catalog.md`.

The plan files each wave was executed from are deleted; `git log` holds them.

## The shape of the work

Over 2026-09-05 and 2026-09-06 the SQLite index was replaced by a native_db (redb) plus
Tantivy catalog on a worktree branch, merged to `master` at `8757c76`, tuned in two
performance waves, and stripped to one record version. Each wave was one plan file, one
fresh implementer agent, a verification pass by the dispatcher, and a review.

Waves 1 to 5 built the catalog and its search index. Wave 6 added the `primary` mark that
lets a query skip the collapse. Waves 7 to 14 moved the routes, the faces, user data,
loadouts and jobs, hashing, peers and finally the SQLite store itself. The two performance
waves and the single-version wave followed the merge.

## The scale table

Synthetic loose source of header-only safetensors files, 1,000 per directory, one artifact
per file, release build, `crates/hoard-index/tests/scale.rs` driven by `HOARD_SCALE_ENTRIES`.

Waves 1 to 3 measured `search`, which spoke Tantivy's syntax and returned a page and a total.
Wave 4 removed it; the `query` rows also count five facet dimensions and the distinct nodes,
so the two sets are not comparable and both were kept. The w11 column is `HashPolicy::Never`,
so it hashes nothing; the merged column (`8757c76`) is the same condition, while the w6
column beside it still hashed, so only the 100k pair is like-for-like.

| measure | 100k w1 | 100k w2 | 100k w3 | 100k w4 | 100k w5 | 100k w6 | 100k w11 | 100k merged | 1M w1 | 1M w2 | 1M w3 | 1M w4 | 1M w5 | 1M w6 | 1M merged |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| scan wall | 5.226065725s | 5.900763157s | 5.476153652s | 5.657892353s | 5.672653469s | 5.623549612s | 6.618210648s | 6.722465445s | 57.77687964s | 75.282972971s | 85.709219721s | 85.449911878s | 84.603774091s | 85.078043138s | 98.691301535s |
| peak rss growth during scan | 198 MB | 292 MB | 253 MB | 254 MB | 254 MB | 254 MB | 275 MB | 275 MB | 1560 MB | 842 MB | 731 MB | 741 MB | 732 MB | 732 MB | 735 MB |
| rebuild wall | 596.949484ms | 440.388181ms | 802.170108ms | 943.869337ms | 883.471396ms | 1.076870421s | 1.135933202s | 1.255604558s | 4.40112116s | 5.294884595s | 8.497179982s | 10.39137641s | 10.551732892s | 11.886669682s | 12.5640437s |
| peak rss growth during rebuild | 34 MB | 54 MB | 52 MB | 47 MB | 48 MB | 46 MB | 46 MB | 45 MB | 0 MB | 0 MB | 0 MB | 0 MB | 0 MB | 0 MB | 0 MB |
| on disk, apparent | 518 MB | 261 MB | 261 MB | 265 MB | 265 MB | 265 MB | 265 MB | 265 MB | 4138 MB | 4138 MB | 4138 MB | 4173 MB | 4173 MB | 4174 MB | 4175 MB |
| on disk, allocated | 288 MB | 243 MB | 213 MB | 216 MB | 216 MB | 216 MB | 229 MB | 229 MB | 2983 MB | 2476 MB | 2112 MB | 2148 MB | 2148 MB | 2149 MB | 2242 MB |
| search offset 0 | 10.577049ms | 12.251924ms | 9.756606ms | removed | removed | removed | removed | removed | 189.855663ms | 174.943704ms | 216.076995ms | removed | removed | removed | removed |
| search deep offset | 9.144823ms | 9.106715ms | 8.649164ms | removed | removed | removed | removed | removed | 197.536743ms | 190.89815ms | 217.117028ms | removed | removed | removed | removed |
| search filtered | 7.200678ms | 7.238805ms | 6.967226ms | removed | removed | removed | removed | removed | 177.39202ms | 168.239764ms | 188.590775ms | removed | removed | removed | removed |
| query empty by name | | | | 221.111746ms | 33.894331ms | 13.27674ms | 13.43545ms | 13.28162ms | | | | 3.011389042s | 577.455193ms | 125.506335ms | 122.717398ms |
| query deep offset | | | | 212.094065ms | 34.319659ms | 13.447428ms | 13.634569ms | 13.43946ms | | | | 2.982565132s | 534.299433ms | 125.852064ms | 123.029078ms |
| query role:lora with facets | | | | 135.635441ms | 28.611903ms | 16.175599ms | 15.641774ms | 15.190983ms | | | | 2.016078812s | 493.170639ms | 159.062959ms | 145.8286ms |
| query filtered | | | | 300.337912ms | 41.425115ms | 25.132866ms | 24.692991ms | 24.023572ms | | | | 4.384522112s | 873.916709ms | 251.142862ms | 235.649166ms |
| query node:self with facets | | | | | | 58.87439ms | 64.457049ms | 64.255697ms | | | | | | 1.082111424s | 1.121332943s |
| peak rss growth during queries | | | | 113 MB | 41 MB | 1 MB | 1 MB | 1 MB | | | | 1190 MB | 409 MB | 0 MB | 0 MB |
| peak rss growth during the collapse query | | | | | | 42 MB | 34 MB | 33 MB | | | | | | 300 MB | 262 MB |
| rescan wall (1,000 changed) | | | 812.140523ms | 816.293704ms | 898.947574ms | 517.541105ms | 631.498106ms | 642.38082ms | | | 28.5581122s | 29.606316494s | 31.851403311s | 6.241905939s | 7.364252708s |
| refresh wall | | | 28.341576ms | 31.047147ms | 31.89983ms | 27.67862ms | 31.797068ms | 21.002736ms | | | 28.598925ms | 35.828931ms | 33.576794ms | 29.871137ms | 27.537449ms |

An empty cell is a measure that did not exist in that wave.

Wave 4's collapse-everything collector is the 3 s and 4.4 s row at 1M. Wave 5 bounded what a
segment retains and took it to half a second. Wave 6 added the `primary` mark and took the
four primary-path queries to about 125 ms with no measurable memory. The collapse path was
left at a second and 300 MB and became performance wave 1's subject.

## The SQLite baseline

Measured once, on 2026-09-05, to establish the baseline, and its harness was removed. At
100k the SQLite index scanned in 1832 s with 356 MB of peak RSS growth and 136 MB on disk,
searching in 215 to 336 ms. Its 1M scan was killed after 52 minutes roughly one eighth of the
way in. The native catalog's first measurement at the same size, before bounded units and
narrow records landed, was 3.65 s at 100k and 37.9 s at 1M.

## Performance wave 1, commit 7737aeb

Routes over a synthetic tree of 100 loose repositories of 1,000 headers each, before column
`d9e1636`, three runs, median:

| route | before | after |
| --- | --- | --- |
| `GET /api/v1/repos?limit=1` | 63.3 ms | 0.52 ms |
| `GET /api/v1/repos?limit=100` | 261.0 ms | 0.83 ms |
| `GET /api/models?limit=1` | 61.5 ms | 0.75 ms |
| `GET /api/models/org0/repo000` | 3.58 ms | 3.35 ms |
| `HEAD /org0/repo000/resolve/main/w0.safetensors` | 2.18 ms | 0.63 ms |
| `GET /api/v1/sources` | 38.9 ms | 0.45 ms |
| `GET /api/v1/search?q=` | 14.6 ms | 14.2 ms |
| `GET /api/v1/search?q=node:self` | 68.3 ms | 41.7 ms |
| `GET /api/v1/search?q=role:lora` | 17.0 ms | 16.9 ms |
| `GET /api/v1/triage?limit=20` | 73.8 ms | 78.5 ms |

The three listings and `sources` each used to read every entry of every repository to decide
presence, count files or count owed hashes, and now read a number off the record they already
had; `resolve` became one location-key lookup. Four records grew: `Repository` carries
`present_files` and `present_bytes`, `Source` carries `pending_hashes`, `Node` carries
`filed`, `Entry` carries `location`. `hoard doctor` gained the `summary` check that recounts
every artifact record and reports drift.

At 1M the collapse query fell from 1.12 s and 262 MB to 511 ms and 35 MB: the collapse path
keeps a 40 byte row per matching document rather than an 88 byte hit in a map per segment,
reads no facet ordinal until one row per artifact is settled, and a facet pass keeps no
candidates. Allocated disk rose 2242 to 2564 MB, which is the unique `location` key.

## Performance wave 2, commit 64e11af

Before column `a4dd324`, measured in the same session. Two rows are the wave: the triage
queue read every artifact record into memory before keeping twenty (92.1 ms to 9.35 ms), and
the usage tab read one repository's whole log three times (3.37 ms to 1.39 ms). Everything
else moved by less than the spread of its own three runs.

The resolve loop did not move: 200 sequential requests take 0.98 s before and 0.99 s after,
and 400 requests with 20 in flight take 0.81 s either way. A streamed response costs about
40 ms in HTTP framing whatever the catalog does. What the wave removed is measurable at the
catalog: 2,000 served responses cost 1.29 s through `log_usage` plus `bump_artifact_use`, and
23.8 ms through `log_usage_batch`.

Hashing, page cache warm: 100,002 files under 1 KB take 13.58 s against 16.00 s, because a
file under the LFS threshold owed a sha256 and a git blob sha1 and was read once for each and
is now read once with both hashers fed from the same buffer. 64 files of 64 MiB barely move,
because those bytes were already resident.

Allocated disk at 1M fell 2564 to 2276 MB: `Entry` dropped the `node` secondary key and
stores a path only when the bytes do not sit at the repository's root joined with the
relative path. The rescan's memory fell 528 to 359 MB: the stamp map is keyed by the xxh3 of
the path rather than by the path, and a repository's rows are read one at a time.

Three writers that used to leave the totals behind no longer do. Retiring a path recounts
what its artifact still holds; a verdict measures the record before and after, because
settling a role at full confidence takes it out of the triage total.

## Workstation, merged, commit 8757c76

The HuggingFace cache, the ComfyUI tree, the LM Studio tree and one git source whose root no
longer existed and was skipped with a warning.

First scan 5 m 42.385 s wall, `unchanged=728 changed=0 added=0 absent=0 hashed=445 merged=2`;
almost all of it is the 445 files read end to end for a content hash under
`hash_policy = "background"`. Second scan immediately after: 0.229 s, hashing nothing.
Catalog on disk 6.8 MB, of which `catalog.redb` is 6.2 MB and the published search directory
572 KB. `hoard ls` queries were 15 to 17 ms each, which is process start and opening two
files rather than the query; routes on a running server were 0.5 to 2.4 ms.

The single-format catalog scanned at `1ba8475` reproduced those numbers within their own
spread, which is the point recorded in `docs/native-catalog.md`: one shape per record costs
no more and no less on disk than the ladder of versions did, because only the current shape
was ever written.

## Carried across at integration

The worktree started at `647cd99`. Commit `c0d877e` added scheduled cache eviction on the
main branch while this one was being built, including two SQLite functions this branch had
already deleted. The merge ported them to the catalog: `Catalog::served_cache_entries`
returns every SERVED entry of the local node with no file of its own on disk, and
`Catalog::retire_entries` moves such rows back to REMOTE with no pull progress and the
preference REMOTE carries, keeping the row and logging its artifact for the search index.
`cache::reconcile::reconcile_index` and `cache::sweep::spawn_eviction` take the catalog, and
the reconciliation refreshes the search index after a retirement so a query does not read
`stale`. The reconciliation still walks the cache tree between the read and the write rather
than holding the catalog open across it.
