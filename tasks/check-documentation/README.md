# Check documentation

Validate repository documentation and task entry points without building Rust or starting
services. Requires Node.js 24+ and no installed packages.

```sh
node tasks/check-documentation/check.mjs
```

[check.mjs](check.mjs) checks Markdown file links and heading anchors, rejects obsolete
repository paths, checks file references in Rust documentation comments, and syntax-checks
task JavaScript entry points. Generated capture reports and immutable HTML prototype assets
are not rewritten or interpreted as current instructions.
External URLs are not fetched. The task is safe to run while application services are active.

Run this check after moving pages or task tooling. It complements review of the actual
contracts; a valid link does not prove that prose matches implementation.
