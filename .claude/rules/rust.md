---
paths:
  - "crates/**/*.rs"
  - "src/**/*.rs"
---

# Rust rules for hoard

- Public items get `///` docs only when the name and types do not convey the contract.
- `#[cfg(test)] mod tests` in the same file; Arrange-Act-Assert; `tempfile` for disk, `wiremock`
  for upstream HTTP; never read real model files in tests.
- Newtypes for ids and hashes: `RepoId`, `CommitSha`, `Sha256`, `BlobId`, `ArtifactId`,
  `NodeId`. Never pass a bare `String` where one of these exists.
- Handlers: `async fn(State<AppState>, ...) -> Result<Response, AppError>`. No `unwrap`.
- Blocking work (the catalog, hashing, header parsing) runs in `spawn_blocking` or `rayon`.
- Streaming bodies: never buffer a file; stream from the block cache.
- Header names in Hub responses are lower-case and must match `docs/api/hub-compat.md`.
- Every new module is listed in `docs/architecture.md` under the crate tree.
