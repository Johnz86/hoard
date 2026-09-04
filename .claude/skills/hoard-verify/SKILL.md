---
name: hoard-verify
description: Run the full hoard verification suite (fmt, clippy with -D warnings, tests, doc build) and fix everything it reports before declaring a task done.
allowed-tools: Bash, Read, Edit, Grep
---

Run these in order from the workspace root and stop at the first failure:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo doc --workspace --no-deps
```

For each failure: read the diagnostic, fix the root cause (never silence a lint with
`allow` unless AGENTS.md permits it), and rerun from the failing step. When all four pass,
report the test count and any warnings from `cargo doc`.

If a Hub route changed, also run the contract smoke test when a server is reachable:

```sh
HF_ENDPOINT=http://127.0.0.1:7099 hf download <repo-in-index> --local-dir /tmp/hoard-smoke
```

Never mark a task complete with a failing or skipped step; say which step failed and why.
