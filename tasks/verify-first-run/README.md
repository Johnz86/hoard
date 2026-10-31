# Verify first-run behavior

Run the browser acceptance journey against an isolated node. It covers empty arrival, explicit
searches, owner-token access, invalid-path recovery, empty folders, polling focus and form
position, sharing settings, restart, shared links and removal of the last source.

## Requirements and command

Use Node.js 24+, Git, a Chromium-based browser and the debug Hoard binary. Brave at
`/usr/bin/brave-browser` is the default; `HOARD_BROWSER` selects another compatible binary.

```sh
nice -n 10 cargo build --workspace -j 2
node tasks/verify-first-run/verify.mjs
```

The entry point is [verify.mjs](verify.mjs). It resolves the repository root from its location.
No npm installation, audio model or frontend build is required.

## Isolation and evidence

The task creates synthetic resources, its own catalog, configuration, owner token and browser
profile in a temporary directory. It binds an available loopback port, disables upstream and
discovery, limits worker counts and stops only its own processes. It never uses production
configuration or catalogs.

Successful runs print the evidence directory containing light/dark screenshots at 1180/1600px,
a verification JSON file and server logs. Failed runs retain evidence for diagnosis. No data
is written into the checkout. The task is an acceptance check for this workflow, not a
replacement for [broader UI review](../../docs/contributing/ui-changes.md).
