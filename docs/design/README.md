# Design status and next work

Reviewed on 2026-10-31, initially against `ce1c098`, then against the Git-project delivery.
The navigation, family rail, shelves, preferences, combined Sources view and Git-project
search and browsing are implemented. The subsequent cohesion correction integrates projects
into the established Index/detail layouts and delivers source removal and scan-history cleanup.

## Current references

- [UI specification](ui-spec.md): the implemented UI and the retained prototype dimensions.
- [JSON API](../api/hoard-api.md): the routes available to the UI.
- [Handoff](../handoff.md): implementation history and the latest review.

The imported [design handoff](handoff/README.md) remains unchanged. Later accepted changes
such as INDEX / SOURCES navigation are recorded in the UI specification. The historical
reviews below explain earlier decisions; their claims about current behavior refer to their
stated commits, not to today's application.

## UI change workflow

The owner accepted the cohesion correction in `82bc336`: Index, Sources and project details
now behave as expected within Hoard's design. Use that implementation as the baseline for
future extensions. The initial Git delivery demonstrated a working find/review/clone journey
but failed broader design acceptance. Functional success and visual/interaction consistency
are separate review requirements.

### Before implementation

1. Read the current specification and inspect the actual affected screens and their renderers.
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

### During implementation and review

- Reuse shared renderers, layout classes and tokens. Add only the type-specific facts,
  content and actions needed. Keep the existing density, spacing, columns and action placement.
- Keep typed form values, focus, confirmation, selection and scroll stable through polling
  and rerenders. Exercise navigation while requests are pending, not only after they finish.
  Cancel/Escape returns to a sensible control; returning from detail preserves loaded pages.
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
  contract; a narrow journey review missed the original regression.
- Use synthetic fixtures, temporary catalogs and ephemeral loopback ports. Keep production,
  port 8080 and live sources untouched. Limit build/hash/test concurrency on the shared
  machine, and stop only the processes created for verification.

Before reporting completion, run the applicable checks in `CLAUDE.md`, including release
asset tests for web/server changes. Record the observed behavior, review outcome and any
remaining limitations in the handoff. Keep current contracts separate from dated proposals;
do not present a superseded plan or a narrow test result as current design approval.

## What the code currently delivers

| Area | Implementation | State |
| --- | --- | --- |
| Navigation | `web/js/app.js`, `web/js/sources.js` | INDEX / SOURCES; old nodes, scans and jobs routes open Sources |
| Family rail | `web/js/facets.js`, `hoard-index/src/search/brands.rs` | Flat MODEL FAMILY rows; full role labels |
| Shelves | `web/js/shelves.js` | Search controls hold the shelf picker and add action |
| Preferences | `web/js/app.js`, `web/js/theme.js` | Theme and display settings in preferences |
| Add a folder | `api/jobs.rs`, `state.rs`, `config.rs` | Saves UI sources in `sources.json`, queues a scan, restarts the watcher |
| Remove a folder | Source row REMOVE / CONFIRM / CANCEL | Persistent removal, original files kept, queued scans and watchers coordinated |
| Clear completed scans | Activity DISMISS / CLEAR FINISHED SCANS | History-only cleanup; active jobs protected |
| Git projects | `GET /api/v1/projects`, `web/js/projects.js` | Main search and Sources lead to README, files, revisions and clone/share controls |
| Git serving | `hoard-server/src/git/` | Canonical `/git/projects/{id}.git` clones projects independently of Hub namespaces |

Rust paths in this table are under `crates/`; the folder-management files are under
`crates/hoard-server/src/`.

## Original findings for the reported Git repository

The review before implementation used read-only checks against `http://127.0.0.1:7099`:

- Sources lists `/home/johnz86/GIT/pose-character`, resolved layout `git`, 41 files.
- `GET /api/v1/repos?q=pose-character` returns `models/pose-character`, default ref `master`.
- `GET /api/v1/search?q=pose-character&limit=1` returns no artifacts.
- `git ls-remote http://127.0.0.1:7099/pose-character.git HEAD` returns
  `b6a1ab9c1ceaf37abd98262166048fe0661866f5`.
- `GET /api/v1/repos/models/-/pose-character` returns its detail and a valid `pull.git`.

The folder was added and the Git face works. Artifact search does not list an ordinary
source-code repository without artifacts. The missing repository browser makes a successful
addition look unsuccessful. This repository can already be acquired with:

```sh
git clone http://127.0.0.1:7099/pose-character.git
```

Its existing detail page can be opened directly at
`http://127.0.0.1:7099/#/repo/models/-/pose-character`. The `-` is significant: the API
requires an org segment and `api/common.rs::repo_id` accepts `-` or `_` for an orgless repo.
The UI's URL builders originally emitted or omitted an empty org segment, producing an API
path that returned 404. The implementation now consistently uses `-` on artifact repository
routes. Projects use neutral `#/project/{id}` routes and do not expose a model namespace.

Git verification was limited to the ref advertisement; no clone, scan, download or production
mutation was performed during this review.

## Initial Git-project delivery (UI superseded below)

A prospective-user agent reviewed the find, review and clone journey, but that narrow review
missed the broader layout regression. The UI description in this section is historical; the
cohesion correction below supersedes it. This is a resource catalog and read-only sharing
interface; it adds no forge, editing, issues or pull-request features.

- Main search returns a separate Git-project group, even when there are no model artifacts.
  Name, reference, source, default-revision file paths and committed README text are indexed.
  The transient Tantivy index is built lazily and refreshed after the scan lane finishes;
  searches do not reread Git objects on each keystroke.
- Sources offers BROWSE PROJECTS, scoped to the selected Git folder. Project results paginate.
- `#/project/{id}` opens a neutral Git-project view with source, node, file count, revision,
  README, FILES, COPY CLONE and COPY PROJECT LINK. Text previews are escaped; README rendering
  disables unsafe URL schemes. Relative README file links open the file browser.
- README and file views read the selected commit's objects, including from bare repositories.
  Dirty working-tree changes are excluded. Binary and oversized files have explicit states;
  previews are limited to 256 KiB and individual downloads to 16 MiB. Clone transfers the repo.
- The canonical clone endpoint uses the catalog repository ID, so ordinary Git projects do
  not depend on the internal Hub namespace. Clone commands quote the URL and destination.
- The stored catalog format remains 19. Existing scanned Git repositories need no rebuild.

The isolated browser test covered project-name and README searches, README rendering, a
relative documentation link, nested text, binary and oversized files, file-list scrolling,
branch changes, shared-link reload, Sources navigation and an actual HTTP clone. Regression
tests also cover bare repositories, safe paths, shell quoting and refresh after a rescan.
No production service, configuration or live catalog was modified. LAN sharing uses the
configured reachable `public_url`; the local deployment's loopback setting was left intact.

## Cohesion correction and broad acceptance

The owner rejected the separate project-results block and tall project header because they
changed both Index and Sources layouts. The correction uses the established result table and
cards for both kinds, with one cursor, inspector, sort, scrollbar and pagination. Project
details reuse the existing detail header, tabs, revision bar and right rail. Sources keeps
BROWSE / SCAN / REMOVE in each row; BROWSE opens a normal source query in Index rather than
inserting another result section. Model inspection, loadouts and existing density/theme
controls retain their patterns. Shelf counts agree with the resources their queries show.

REMOVE arms CONFIRM / CANCEL in place and says files stay on disk. UI and TOML folders stay
removed across restart; re-adding restores them. Finished-scan cleanup is separate and cannot
delete active/queued work. Removal survives caller disconnection and startup repairs persisted
intent. Existing catalog record shapes and format 19 are unchanged.

Acceptance compares artifact-only layouts at 1600/1180px across two themes, two densities,
inspector on/off and TABLE/CARDS (32 combinations), then mixed-resource click/double-click,
J/K/Enter/Y/L behavior, sorting/filtering/inspector, source-query browsing, project files/refs,
scroll and deep links. Sources tests preserve form and confirmation focus through polling,
remove UI/TOML sources while keeping files/other sources, restart, re-add and clone. Async
removal/navigation and loaded-page return are included. Verification uses isolated data and
ephemeral ports, never production or port 8080.

## Removal acceptance requirements retained from the review

1. **Remove a folder from Sources.** Add a named-source removal action with confirmation
   that it stops indexing and serving that source and removes its catalog metadata, while
   leaving the original files intact. Address UI-added and TOML-configured sources explicitly:
   both use persistent exclusions in `sources.disabled.json`, while UI definitions can remain
   masked in `sources.json` and TOML stays untouched. Re-adding clears the exclusion and
   updates the managed definition so the source stays restored after restart.
   Coordinate admission, queued/running scans and watcher reconfiguration so no stale scan
   can recreate removed rows. Reuse the catalog's existing retirement path and refresh search
   and derived counts. Do not trigger a full scan of unrelated sources as a removal shortcut.
   Source names, unlike the current positional `s_` IDs, are suitable route identities.
2. **Clear completed scan jobs from Activity.** Add a dismiss action for finished scan
   jobs and a clear-completed-scans action. These remove job history only, never folders,
   indexed files or catalog contents. Refuse deletion of running or queued jobs; retain their
   existing cancel action. Persist the cleanup and test final-state checks, authentication
   and repeated requests. The owner explicitly requested both this and folder removal.
3. **Extend the removal acceptance journey using synthetic repositories.** Add a folder, wait for its
   scan, find its repository, open it and clone through an isolated server. Remove the source,
   verify that source-specific catalog rows and serving disappear, then restart and verify
   it stays removed. Cover shared artifacts, a queued scan, authentication and stale requests.
   Clearing completed Activity jobs is a separate action from removing a source.

Run development servers with temporary data directories and an available loopback port.
Port 8080 is occupied by production. Do not restart services, reuse a live catalog, scan the
real caches, or launch unrestricted build/hash parallelism for this work.

## Historical research and decisions

These documents are retained for their evidence and decision history. Read them in this
order; later decisions supersede earlier proposals. Screenshot paths and temporary evidence
locations in these reports are historical and their images are not included in this checkout.

| Document | Role and later outcome |
| --- | --- |
| [Navigation review](catalog-navigation-review.md) | Original proposal; its two-tier brand hierarchy was rejected later |
| [ComfyUI taxonomy](comfyui-model-taxonomy.md) | Research behind the source-layout facet; source snapshots are dated evidence |
| [IA recommendations](catalog-ia-recommendations.md) | Proposal reconciled by the improvements plan |
| [Improvements plan](catalog-improvements-plan.md) | Initial four waves landed as `787e65d`, `a9e8ce7`, `55fa320`, `82af8b5`; the hierarchy and filter were later replaced |
| [Brand and hashing correction](brand-rail-and-hashing-refactor-plan.md) | Flat rows and bounded hashing landed; labels and projector pairing were refined later |
| [Consistency review](catalog-consistency-review.md) | Findings against `5de7193`; resolved by the following plan |
| [Catalog UX resolution](catalog-ux-resolution-plan.md) | Waves landed as `19f1d5e`, `3ff3d01`, `38d57ee`, `b8c1cc2` |
| [Acquire journey resolution](acquire-journey-resolution-plan.md) | Subsequent waves and follow-ups landed through `c91af45`; its old “still open” list is not a current backlog |

In particular, do not rebuild the old nested rail, restore its section filter, or repeat
historical catalog format bumps. Later commits already addressed the acquire plan's
declared-set totals, triage predicate and lineage display findings. Future work starts from
the current code and the gaps above.
