---
name: hoard-verify
description: Run the full hoard verification suite (fmt, clippy with -D warnings, tests, workspace build, the release asset test, doc build) and fix everything it reports before declaring a task done.
allowed-tools: Bash, Read, Edit, Grep
---

Run these in order from the workspace root and stop at the first failure. Every one of them
in the foreground; never leave anything running.

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace
```

`cargo fmt --check` should already pass, because a `PostToolUse` hook runs `rustfmt` on every
edited `.rs` file. If it does not, run `cargo fmt --all` and rerun the check.

Additionally:

- **After any change under `crates/hoard-server/` or `web/`**, run the release asset test.
  `web/` is read from disk in a debug build and embedded in the binary in a release one, so
  the four checks above never touch the code a shipped binary runs:

  ```sh
  cargo test --release -p hoard-server --lib assets::
  ```

- **At the end of a wave**, once, run the doc build and report its warnings:

  ```sh
  cargo doc --workspace --no-deps
  ```

For each failure: read the diagnostic, fix the root cause (never silence a lint with `allow`
unless AGENTS.md permits it), and rerun from the failing step.

If a Hub route changed, also run the contract smoke test when a server is reachable, with the
server and the client in one foreground command under `timeout`:

```sh
HF_ENDPOINT=http://127.0.0.1:7099 hf download <repo-in-index> --local-dir <scratchpad>/smoke
```

When every step passes, report the final output line of each: the test counts, the clippy
line, and any warnings from `cargo doc`. Never mark a task complete with a failing or skipped
step; say which step failed and why.
