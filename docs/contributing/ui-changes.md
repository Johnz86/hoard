# Reviewing UI changes

A feature must complete its own journey and fit the broader application. Review both
functional behavior and visual/interaction consistency.

## Before implementation

1. Read the [UI specification](../design/ui-spec.md) and inspect affected screens and renderers.
   Capture before screenshots with representative existing data, including an artifact-only
   Index. Identify the current table/cards, inspector, detail tabs, revision bar, right rail,
   row actions, keyboard controls and scroll containers that the feature can reuse.
2. Describe the user's complete journey and the existing behavior it must preserve. Hoard
   helps people find, review and acquire resources over a network. A Git project is a resource
   with its own facts and actions; an internal Hub namespace does not make it a model or
   dataset. Keep model-only controls scoped to models. Repository hosting features are outside
   this resource-sharing journey.
3. Map each new action onto an existing surface before introducing markup or CSS. A new
   resource type belongs in the shared result list, cursor, selection, inspector, sort and
   pagination. Sources BROWSE opens an Index query. Detail content uses the shared header,
   tabs and right rail. Additional result groups, nested browsers and oversized headers need
   a concrete user need and an explicit design decision.
4. Write observable acceptance checks before coding, including existing-resource regressions.
   Resolve material ambiguity in user language: removing a scanned folder and clearing a
   completed scan are distinct actions. Define what each action changes, preserves and does
   after restart; endpoint success alone is insufficient evidence.

## During implementation and review

- Reuse shared renderers, layout classes and tokens. Add only the type-specific facts,
  content and actions needed. Keep the existing density, spacing, columns and action placement.
- Keep typed form values, focus, confirmation, selection and scroll stable through polling
  and rerenders. Exercise navigation while requests are pending, not only after they finish.
  Cancel/Escape returns to a sensible control; returning from detail preserves loaded pages.
- Test a pending save that finishes after its drawer closes or reopens. Update only the
  original mounted editor, retain newer drafts and never focus detached controls.
- Measure useful capacity with a populated collection and a short viewport. Closed model
  rows stay condensed; details and edit/remove actions remain accessible by click and keyboard.
  Compare adjacent input/button heights and role/name column gaps, not just screenshots.
- Verify resource totals, facets and saved-shelf counts against the displayed list. Test
  mixed-resource ordering and pagination so resources cannot disappear, repeat or be skipped.
- For removal, verify the visible action and the full lifecycle: persistent intent, queued
  and running work, watcher exclusion, caller disconnection, restart and re-add. Preserve
  original files, other sources and shared artifacts. Scan-history cleanup changes history
  only and protects work still owned by the scan lane. Reuse existing catalog retirement,
  counter and search-refresh paths; avoid rescanning unrelated sources as a shortcut.
- Review screenshots beside the baseline and use both mouse and keyboard. For shared layout
  changes, cover 1600/1180px, light/dark, Condensed/Roomy, inspector on/off and TABLE/CARDS
  (32 combinations). Check overflow, row height, action fit and scroll ownership. For a
  smaller change, choose affected states and explain the coverage rather than rerunning
  unrelated journeys.
- Review the whole application as a user, including existing artifact inspection/loadouts,
  source-query browsing, README/file/ref navigation, deep-link reload and actual acquisition.
  A clone advertisement or copied command does not prove that cloning works. If another
  reviewer or agent is involved, give them both the feature journey and the broader design
  contract so both kinds of acceptance are covered.
- Exercise nested folders, duplicate basenames and renamed copies across sources. The label,
  name sort, matched location, copied command, repository link and loadout destination must
  describe the same selection. Preserve deliberate client-path edits during rescans.
- Use synthetic fixtures, temporary catalogs and ephemeral loopback ports. Keep production
  services and live sources untouched. Limit build/hash/test concurrency on the shared
  machine, and stop only the processes created for verification.

Before reporting completion, run the applicable checks in
[CONTRIBUTING.md](../../CONTRIBUTING.md), including release asset tests for web/server changes.
Update the owning reference or task guide when behavior or commands change.
Keep current contracts separate from dated proposals;
do not present a superseded plan or a narrow test result as current design approval.
