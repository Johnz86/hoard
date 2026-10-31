---
name: hoard-measure
description: The measurement protocol for hoard performance work - the synthetic tree, the fixed route list, the scale harness at 100k and 1M, the like-for-like rule, and the shape of the numbers that go into docs/performance/catalog-measurements.md.
allowed-tools: Bash, Read, Grep, Glob, Edit, Write
---

Numbers without this protocol are not evidence. Day-to-day timings on this workstation drift
by more than most of the effects being measured, so a "before" number quoted from a document
is not a comparison.

## The like-for-like rule

**Measure before and after in one session, on one machine, over one tree.** A reviewer or an
implementer who needs a "before" number rebuilds the previous commit in a scratch clone under
the scratchpad and measures both runs back to back. Never compare a fresh run against a
number already written in `docs/performance/catalog-measurements.md`; quote that number as history and say
which machine and commit it came from.

State the commit for every column. A table without commits is not reproducible. **Cite a
commit that is on `master`**: a wave's per-task checkpoints are squashed away when it lands, so
a hash from the wave branch names nothing afterwards. The before column is the gate commit; the
after column is the wave commit itself, named by its subject until it has a hash.

## Process hygiene while measuring

- Every command in the foreground. Never start or leave a background process.
- A server under test runs under `timeout` **in the same shell command** as the curls that
  hit it, so it cannot outlive the measurement. Confirm with
  `pgrep -af 'target/release/hoard'` afterwards and report that it found nothing.
- Benchmarks and the scale harness run **one at a time, and nothing else runs beside them**.
  A `cargo build`, a `cargo test` or a second harness competing for the disk and the page
  cache voids the numbers. If you are reviewing while someone else measures, wait; if you are
  measuring, do every build before the first timed run.
- Never set `RUSTFLAGS` and never `target-cpu=native`.
- **This workstation is heterogeneous, and an unpinned latency reading carries a 1.68 factor
  from core placement alone.** It has ten Cortex-X925 at 3.90 GHz on CPUs 5-9 and 15-19 and
  ten Cortex-A725 at 2.81 GHz on CPUs 0-4 and 10-14. The scheduler places a run on either
  class, so repeated unpinned readings of the same work are bimodal rather than noisy: at 1M
  the same midpoint query measured 76.3 ms on the fast class and 127.9 ms on the slow one.
  Two readings that differ by less than that factor are not a difference. **Pin a latency
  comparison to one class** and say which:

  ```sh
  taskset -c 5-9,15-19 <the timed command>    # the ten Cortex-X925
  taskset -c 0-4,10-14 <the timed command>    # the ten Cortex-A725
  ```

  This is what a concurrency reading of 632 ms at `0935fde` turned out to be: not a lock and
  not bandwidth, but eight queries landing across both classes. The curve that settled it is
  in `docs/performance/catalog-measurements.md` under "Query latency against concurrent thread count".
- Everything temporary - synthetic trees, scratch clones, the before and after files - goes
  under the scratchpad directory the harness names in your system prompt, never inside the
  repository. Delete a large tree when you are done with it. `git status` must be clean when
  you are done.

Use `tasks/benchmark-catalog/README.md` for the current entry point and isolation rules.

## The synthetic tree

100 loose repositories of 1,000 small valid safetensors headers each: 100,000 files, one
artifact per file. The generator in `crates/hoard-index/tests/scale.rs::header` is the model
for a header - a little-endian u64 length, then a JSON object with `__metadata__` and one
tensor entry. Vary the key family and the shape across files so inference has something to
do.

Two things belong beside the tree because the fixed route list below needs them, and the
numbers in `docs/performance/catalog-measurements.md` were taken with them:

- **A git clone source**, one small repository with a commit, added as a second
  `[[index.sources]]` with `kind = "git"`. A loose repository has no revision sha, so
  `resolve` of a loose file answers 404 whatever the catalog holds and its row would measure
  the 404 path.
- **A seeded usage log**, about 2,000 served responses on one repository, before the usage
  route is timed. An empty log measures nothing.

Write a `hoard.toml` beside the tree:

```toml
[server]
bind = "127.0.0.1:7311"
data_dir = "<tree>/data"

[node]
name = "bench"
serve = true
discovery = "static"

[index]
hash_policy = "never"

[[index.sources]]
name = "bench"
path = "<tree>/repos"
kind = "loose"
repo_per_folder = true

[mirror]
offline = true
```

`hash_policy = "never"` keeps the run from measuring the disk instead of the catalog, and
`offline = true` keeps it from reaching `huggingface.co`.

`[index] hash_policy` is read by `Catalog::open_or_scan` alone, which is the first open of a
catalog that does not exist yet. `hoard scan` takes its policy from `--hash`, whose default is
`later`, so **the scan must also be given `--hash never`** or it hashes 100,000 files and the
number is the disk.

## The run

```sh
cargo build --release
time ./target/release/hoard --config <tree>/hoard.toml scan --hash never
```

Then the server and the curls in **one foreground shell command**, wrapped in `timeout`, so
the server cannot outlive the measurement. Never leave `serve` running between tool calls:

```sh
timeout 300 sh -c '
  ./target/release/hoard --config <tree>/hoard.toml serve &
  server=$!
  sleep 3
  for route in "/api/v1/repos?limit=1" "/api/v1/repos?limit=100" ...; do
    for run in 1 2 3; do
      curl -s -o /dev/null -w "$route %{time_total}\n" "http://127.0.0.1:7311$route"
    done
  done
  kill $server
'
pgrep -af "target/release/hoard" || echo "no server left"
```

The fixed route list, all eleven, three runs each, median reported, whole request including
the response body. The `HEAD` row is taken with `curl -I`; every other row is a GET:

| route |
| --- |
| `GET /api/v1/repos?limit=1` |
| `GET /api/v1/repos?limit=100` |
| `GET /api/models?limit=1` |
| `GET /api/models/org0/repo000` |
| `HEAD /org0/repo000/resolve/main/w0.safetensors` |
| `GET /api/v1/sources` |
| `GET /api/v1/search?q=` |
| `GET /api/v1/search?q=node:self` |
| `GET /api/v1/search?q=role:lora` |
| `GET /api/v1/triage?limit=20` |
| `GET /api/v1/repos/models/org0/repo000/usage` |

The `resolve` row names the git clone source, not the loose tree, and the usage row names the
repository whose log was seeded. Add a route when the change touches one the list does not
cover, and say you added it.

Write a **before file** and an **after file** under the scratchpad, one line per route per
run, and derive the medians from them rather than from memory.

## The scale harness

`crates/hoard-index/tests/scale.rs` is an ignored test driven by environment variables.
Release, foreground, one at a time, and delete the tree afterwards:

```sh
HOARD_SCALE_ENTRIES=100000 HOARD_SCALE_DIR=<scratch>/scale \
  cargo test --release -p hoard-index --test scale -- --nocapture --ignored
HOARD_SCALE_ENTRIES=1000000 HOARD_SCALE_DIR=<scratch>/scale \
  cargo test --release -p hoard-index --test scale -- --nocapture --ignored
```

It reports scan wall and peak RSS growth, rebuild wall and RSS, disk apparent and allocated,
the query rows, the collapse query, and the rescan and refresh after 1,000 headers are
rewritten. Record every row it prints, both scales, against the previous column.

Four phases are off unless their variable is set, because each costs a run of its own:
`HOARD_SCALE_DELETE` a mass deletion and the rescan after it, `HOARD_SCALE_USAGE` a log of
half a million served rows, `HOARD_SCALE_CONCURRENT` a burst of simultaneous midpoint queries,
and `HOARD_SCALE_BACKGROUND` a rebuild running inside that burst. With
`HOARD_SCALE_CONCURRENT` set, `HOARD_SCALE_CONCURRENT_THREADS` takes a comma-separated list of
thread counts and runs one burst per count against the same seeded catalog, which is how a
latency-against-concurrency curve is taken without reseeding: `HOARD_SCALE_CONCURRENT_THREADS=1,2,4,8`.
Unset, it runs the one burst of eight it always ran. The standalone `concurrent_catalog_queries`
test opens a catalog an earlier run seeded, so seed once per scale and run the counts against it.

Allocated disk is `du` over the output directory; apparent disk sums file lengths, and redb
grows the file in regions, so the apparent number is quantised and the allocated one is what
the disk actually loses.

The one-artifact-per-file tree carries no cards, no hashes and no tensor metadata, so it
cannot show what the cold-record codecs do. `supplemental_catalog_costs` in the same file
seeds those through the catalog directly: 200 repositories of 3 revisions of 50 files with
long paths, blob placements, sha256 and git shas, a card and a tensor summary per artifact.
It prints per-table storage, record payload and inspector timings, and is not a gate:

```sh
HOARD_SCALE_REPOS=200 HOARD_SCALE_DIR=<scratch>/scale \
  cargo test --release -p hoard-index --features fixtures --test scale supplemental \
  -- --nocapture --ignored
```

`--features fixtures` is required: the module is behind that feature and `-p hoard-index`
alone does not enable it, so without the flag the command runs zero tests and says `ok`. A
whole-workspace build enables it through `hoard-server` and `hoard-mirror`, which is why
`cargo test --workspace` counts it among the four ignored.

## The thresholds

At 1M entries, absolute:

- peak RSS growth during the scan under 2 GB
- allocated disk under 1,500,000,000 bytes, reported in exact bytes and MiB
- every query, including the collapse path, under 500 ms
- rebuild no slower than the scan

Say which thresholds the run meets and which it misses, with the number.

## Writing the numbers down

Only a wave that is allowed to write does this. A read-only reviewer reports its numbers and
touches no file in the repository; the before and after files stay in the scratchpad.

Add or replace a section in `docs/performance/catalog-measurements.md` under "Measured" or "Scale". One table
per scale. Every column names the commit it was measured at. Prose after the table says
which rows are the change, which moved by less than the spread of their own three runs, and
which rows are not comparable and why. A row that moved for a reason the change did not touch
is variance and must be called variance, not a result.
