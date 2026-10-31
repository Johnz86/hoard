---
name: hoard-verify
description: Run the applicable Hoard verification checks and report observed results.
allowed-tools: Bash, Read, Edit, Grep
---

Follow `CONTRIBUTING.md` for checks appropriate to the change. Rust changes require formatting,
all-target Clippy with warnings denied, workspace tests/build and the documentation build.
Web/server changes additionally require the release asset tests. Run sequentially, stop on
failure, fix its cause and resume from the failing step. Limit build and test concurrency.

Documentation/task changes use `tasks/check-documentation/README.md` and the affected task
checks. Exercise relocated entry points. Do not start production configuration or reuse a
live catalog to smoke-test a change. Synthetic fixtures, temporary data directories,
available loopback ports and cleanup of only task-owned processes are required.

When Hub routes change, verify the affected contract with isolated clients and fixtures,
including actual byte acquisition. Keep HF_HOME temporary so a client cannot answer from its
own cache. The recording task covers actual Git cloning; broader Hub behavior has its own
Rust route tests and needs a matching fixture for manual acquisition checks.

Some tests are deliberately ignored because they are scale benchmarks, read local model
caches, exercise memory workloads or require multicast. Compare ignored tests with the base
revision rather than trusting a hardcoded count. Do not silently enable real-cache or scale
workloads as part of routine verification.

Report the checks actually run, failures, warnings and limitations. Do not claim current
application approval from a dated report or a narrower promotional recording.
