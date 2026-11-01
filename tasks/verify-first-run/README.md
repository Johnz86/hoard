# Verify first-run behavior

Run the browser acceptance journey against an isolated node. It covers empty arrival, explicit
searches, owner-token access, invalid-path recovery, empty folders, polling focus and form
position, sharing settings, restart, shared links and removal of the last source.
Sharing checks include immediate address changes, continued access through the original
listener without restarting, refreshed cached clone commands, and node-name-only restart
requirements. Address-only saves are captured at both widths and themes.
It also covers creating a named model collection, preparing a client download script and
preserving the drawer shell, draft input and focus. The [loadout task](../verify-loadouts/README.md)
owns the full collection discovery, editing, removal and real client download journey.
Classification coverage uses an additional unclassified model to verify owner-access recovery,
draft/focus preservation, confirmation, matching triage/search/shelf totals, persistence through
rescan and restart, clearing and re-inference, and confirmed noise rejection without file loss.
The inspector review is captured at 1180/1600px in both themes, both densities, table/cards
and inspector on/off, checking that scrolling stays within the inspector and the header
remains visible. Git project controls remain separate from classification controls.

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

Successful runs print the evidence directory containing light/dark setup screenshots at
1180/1600px, loadout screenshots, a verification JSON file and server logs. Failed runs retain
evidence for diagnosis. No data is written into the checkout. The task is an acceptance check
for this workflow, not a replacement for [broader UI review](../../docs/contributing/ui-changes.md).
