# Repository tasks

Each directory owns one repeatable workflow: its purpose, requirements, entry point, inputs,
outputs and verification. Commands are shown from the repository root. The tools locate that
root from their own file paths, so they do not depend on the caller's working directory.

| Task | Entry point | Purpose |
| --- | --- | --- |
| [Check documentation](check-documentation/README.md) | `node tasks/check-documentation/check.mjs` | Local links, anchors, task entry points and obsolete path references |
| [Verify first run](verify-first-run/README.md) | `node tasks/verify-first-run/verify.mjs` | Isolated browser acceptance for setup, configuration and empty states |
| [Verify loadouts](verify-loadouts/README.md) | `node tasks/verify-loadouts/verify.mjs` | Collection discovery, stable editing and real client downloads |
| [Record introduction](record-introduction/README.md) | `node tasks/record-introduction/record.mjs` | Real UI walkthrough synchronized to the supplied narration |
| [Benchmark catalog](benchmark-catalog/README.md) | Rust scale harness | Serial, isolated performance measurements |

Keep fixtures and temporary catalogs outside the repository. Published media belongs in
`docs/media/introduction/`; durable recording inputs live with their task. Add new tasks here
rather than collecting unrelated executables in a generic directory.

After reviewing a run, remove its temporary evidence directory when no longer needed.
Keep durable findings in the owning documentation and published outputs in their documented
locations. Verify owned browser/server processes have exited before deleting profiles or
catalogs; follow [temporary work and cleanup](../CONTRIBUTING.md#temporary-work-and-cleanup).
