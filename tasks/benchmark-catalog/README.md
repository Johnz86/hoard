# Benchmark the catalog

Use the existing Rust scale harness for reproducible catalog measurements. This task is
operator-run and intentionally excluded from ordinary tests; it creates substantial synthetic
data and must not compete with production or other benchmarks.

## Entry points

The harness is [scale.rs](../../crates/hoard-index/tests/scale.rs). Its default fixture is
100,000 synthetic entries; `HOARD_SCALE_ENTRIES` changes the size and `HOARD_SCALE_DIR`
selects scratch storage. Read the selected test before enabling optional phases.

```sh
HOARD_SCALE_DIR=/tmp/hoard-catalog-benchmark HOARD_SCALE_ENTRIES=100000 \
nice -n 10 cargo test --release -p hoard-index --test scale native_catalog_at_scale -j 2 \
    -- --ignored --exact --nocapture --test-threads=1
```

Use a new, task-owned scratch directory for each independently seeded comparison. Tests can
replace their fixture directories: never point this variable at existing user data. Additional
ignored tests cover rescan-only workloads, hashing, retirement, concurrent queries and
supplemental wide/grouped fixtures; inspect their prerequisites in the harness before running.

## Measurement contract

Run baseline and candidate serially on identical fixtures and comparable hardware placement.
Do not set `RUSTFLAGS` or `target-cpu=native`, change the benchmark to favor the candidate, or
mix CPU classes when comparing latency on heterogeneous machines. Record source/binary
identity, command, environment, fixture assertions, elapsed time, memory and allocated disk.
Keep cold/warm and construction/query measurements distinct. Repeat enough to expose spread.

No test node may use a live catalog, cache or fixed production port. Stop only processes you
created. Keep temporary data outside the checkout and remove large scratch trees after retaining
compact results.

Store results as a dated report under `docs/performance/`, with measured commits and workload
limits. [Existing measurements](../../docs/performance/catalog-measurements.md) are historical
evidence, not current guarantees. The catalog write/storage contract belongs in
[the reference](../../docs/reference/native-catalog.md).
