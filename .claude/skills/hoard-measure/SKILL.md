---
name: hoard-measure
description: The measurement protocol for hoard performance work - the synthetic tree, the fixed route list, the scale harness at 100k and 1M, the like-for-like rule, and the shape of the numbers that go into docs/native-catalog.md.
allowed-tools: Bash, Read, Grep, Glob, Edit, Write
---

Numbers without this protocol are not evidence. Day-to-day timings on this workstation drift
by more than most of the effects being measured, so a "before" number quoted from a document
is not a comparison.

## The like-for-like rule

**Measure before and after in one session, on one machine, over one tree.** A reviewer or an
implementer who needs a "before" number rebuilds the previous commit in a scratch clone under
the session scratchpad and measures both runs back to back. Never compare a fresh run against
a number already written in `docs/native-catalog.md`; quote that number as history and say
which machine and commit it came from.

State the commit for every column. A table without commits is not reproducible.

## Process hygiene while measuring

- Every command in the foreground. Never start or leave a background process.
- A server under test runs under `timeout` **in the same shell command** as the curls that
  hit it, so it cannot outlive the measurement. Confirm with `pgrep -f 'hoard.*serve'`
  afterwards and report that it found nothing.
- Benchmarks and the scale harness run **one at a time**. Concurrent runs compete for the
  disk and the page cache and the results are void.
- Never set `RUSTFLAGS` and never `target-cpu=native`.
- Everything temporary goes under the session scratchpad directory, and a large tree is
  deleted afterwards. `git status` must be clean when you are done.

## The synthetic tree

100 loose repositories of 1,000 small valid safetensors headers each: 100,000 files, one
artifact per file. The generator in `crates/hoard-index/tests/scale.rs::header` is the model
for a header - a little-endian u64 length, then a JSON object with `__metadata__` and one
tensor entry. Vary the key family and the shape across files so inference has something to
do.

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

## The run

```sh
cargo build --release
time ./target/release/hoard --config <tree>/hoard.toml scan
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
pgrep -f "hoard .*serve" || echo "no server left"
```

The fixed route list, every one of them, three runs each, median reported, whole request
including the response body:

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

Add a route when the change touches one the list does not cover, and say you added it. A
`resolve` of a loose file answers 404 whatever the catalog holds, because a loose repository
has no revision sha; add a git clone source beside the tree when `resolve` is what the change
is about. Seed the usage log before timing the usage route.

Write a **before file** and an **after file** under the scratchpad, one line per route per
run, and derive the medians from them rather than from memory.

## The scale harness

`crates/hoard-index/tests/scale.rs` is an ignored test driven by two environment variables.
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

Allocated disk is `du` over the output directory; apparent disk sums file lengths, and redb
grows the file in regions, so the apparent number is quantised and the allocated one is what
the disk actually loses.

## The thresholds

At 1M entries, absolute:

- peak RSS growth during the scan under 2 GB
- allocated disk under 1.5 GB
- every query, including the collapse path, under 500 ms
- rebuild no slower than the scan

Say which thresholds the run meets and which it misses, with the number.

## Writing the numbers down

Add or replace a section in `docs/native-catalog.md` under "Measured" or "Scale". One table
per scale. Every column names the commit it was measured at. Prose after the table says
which rows are the change, which moved by less than the spread of their own three runs, and
which rows are not comparable and why. A row that moved for a reason the change did not touch
is variance and must be called variance, not a result.
