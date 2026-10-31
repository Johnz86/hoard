# Design status and next work

Reviewed on 2026-10-31, initially against `ce1c098`, then against the Git-project delivery.
The navigation, family rail, shelves, preferences, combined Sources view and Git-project
search and browsing are implemented. Source removal and completed-job cleanup remain open.

## Current references

- [UI specification](ui-spec.md): the implemented UI and the retained prototype dimensions.
- [JSON API](../api/hoard-api.md): the routes available to the UI.
- [Handoff](../handoff.md): implementation history and the latest review.

The imported [design handoff](handoff/README.md) remains unchanged. Later accepted changes
such as INDEX / SOURCES navigation are recorded in the UI specification. The historical
reviews below explain earlier decisions; their claims about current behavior refer to their
stated commits, not to today's application.

## What the code currently delivers

| Area | Implementation | State |
| --- | --- | --- |
| Navigation | `web/js/app.js`, `web/js/sources.js` | INDEX / SOURCES; old nodes, scans and jobs routes open Sources |
| Family rail | `web/js/facets.js`, `hoard-index/src/search/brands.rs` | Flat MODEL FAMILY rows; full role labels |
| Shelves | `web/js/shelves.js` | Search controls hold the shelf picker and add action |
| Preferences | `web/js/app.js`, `web/js/theme.js` | Theme and display settings in preferences |
| Add a folder | `api/jobs.rs`, `state.rs`, `config.rs` | Saves UI sources in `sources.json`, queues a scan, restarts the watcher |
| Remove a folder | No route or UI action | Missing |
| Clear completed scans | Jobs support listing and cancellation only | Missing |
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

## Git-project delivery

A prospective-user agent reviewed the find, review and clone journey and accepted the final
implementation after the browser checks. This is a resource catalog and read-only sharing
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

## Next implementation, in priority order

1. **Remove a folder from Sources.** Add a named-source removal action with confirmation
   that it stops indexing and serving that source and removes its catalog metadata, while
   leaving the original files intact. Address UI-added and TOML-configured sources explicitly:
   removing a UI source must update `sources.json`; disabling a configured source needs a
   persistent exclusion or an explicit configuration edit so it cannot return on restart.
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
