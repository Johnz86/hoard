# Hoard UI specification

This is the current accepted interface contract. The [product design](README.md) describes
its scope. Original HTML in `docs/design/prototypes/project/` supplies unspecified visual
details; explicit accepted behavior here takes precedence over those prototypes. Preserve
the HTML and support assets when changing application code.

Implementation target: `web/` vanilla JS modules plus `web/css/hoard.css` over Pico CSS.
All values below are already declared as custom properties in `web/css/tokens.css`.
Before changing a screen or adding a resource type, follow the
[UI change workflow](../contributing/ui-changes.md).

## Palette (oklch)

Dark palette, used when the dark theme is selected:

| Token | Value | Use |
| --- | --- | --- |
| `--bg` | `oklch(0.185 0.012 70)` | page, search field, code panels |
| `--panel` | `oklch(0.225 0.012 70)` | header, rails, sticky bars |
| `--panel2` | `oklch(0.265 0.012 70)` | chips, hover, active tab |
| `--rule` | `oklch(0.315 0.012 70)` | borders |
| `--rule2` | `oklch(0.26 0.012 70)` | row separators |
| `--text` | `oklch(0.93 0.008 80)` | primary text |
| `--muted` | `oklch(0.635 0.012 75)` | secondary columns |
| `--dim` | `oklch(0.5 0.012 70)` | labels, placeholders, counts |
| `--amb` | `oklch(0.79 0.155 62)` | the one accent: focus, active, cursor, primary button |
| `--amb-hover` | `oklch(0.86 0.14 66)` | primary button hover |
| `--amb-ink` | `oklch(0.2 0.03 60)` | text on amber |
| `--green` | `oklch(0.76 0.15 155)` | served, present, node online |
| `--red` | `oklch(0.68 0.17 25)` | stale, destructive hover |
| cursor row | `oklch(0.27 0.02 62)` | row under keyboard cursor |
| selected row | `oklch(0.245 0.012 70)` | space-selected rows |
| scrim | `oklch(0.14 0.01 70 / 0.62)` | behind the loadout drawer |
| selection | `oklch(0.5 0.14 55)` bg, `oklch(0.98 0 0)` fg | `::selection` |
| card body text | `oklch(0.84 0.008 80)` | README paragraphs |

Paper palette, used by the light theme:

| Token | Value |
| --- | --- |
| `--paper` | `oklch(0.955 0.006 85)` |
| `--card` | `oklch(0.985 0.004 85)` |
| `--ink` | `oklch(0.215 0.012 70)` |
| `--body` | `oklch(0.36 0.012 70)` |
| `--muted` | `oklch(0.55 0.01 72)` |
| `--rule` | `oklch(0.875 0.008 82)` |
| `--rule2` | `oklch(0.805 0.01 82)` |
| `--amb` | `oklch(0.72 0.17 55)` |
| `--amb-deep` | `oklch(0.5 0.14 50)` |
| `--red` | `oklch(0.58 0.17 22)` |

Role colors (9 plus unknown). `roleColorMode: Monochrome` maps all to `--role-unk`.

| Role key | Label | Color |
| --- | --- | --- |
| `ckpt` | CKPT, checkpoint | `oklch(0.79 0.155 62)` |
| `lora` | LORA, lora / lycoris / ip-adapter | `oklch(0.72 0.15 250)` |
| `ctrl` | CTRL, controlnet / motion module | `oklch(0.74 0.15 155)` |
| `upsc` | UPSC, upscaler | `oklch(0.72 0.15 320)` |
| `vae` | VAE | `oklch(0.72 0.14 200)` |
| `emb` | EMB, embedding / tokenizer | `oklch(0.74 0.15 100)` |
| `ds` | DS, dataset | `oklch(0.7 0.15 22)` |
| `llm` | LLM, language model | `oklch(0.8 0.02 80)` |
| `proj` | PROJ, vision projector | `oklch(0.72 0.15 285)` |
| `unk` | `?`, triage | `oklch(0.6 0.012 75)` |

Sub-roles shown as chips in the taxonomy: IPA (ip-adapter, lora color), TOK (tokenizer, emb
color), MOT (motion module, ctrl color), CMP (COMPLETE, a checkpoint that loads alone, ckpt
color), BB (NEEDS PAIRING, a checkpoint wanting a separate CLIP and VAE, or a vision
projector wanting its host, in the color of the role it sits under).

State colors:

| State | Meaning | Color |
| --- | --- | --- |
| SERVED | this node holds it and serves it on the LAN | `--green` |
| LOCAL | held on this node and served on the Hub face; SERVED marks bytes the mirror fetched | `oklch(0.72 0.012 75)` |
| PULLING nn% | transfer in progress | `--amb` |
| STALE | pinned revision no longer matches upstream ref | `oklch(0.7 0.15 22)` |
| REMOTE | indexed on another node, no bytes here | `oklch(0.6 0.012 75)` |
| TRIAGE | unclassified, needs confirmation | `--amb` |

File states in the repo detail: PRESENT (`--green`), DEDUPED (`--amb`), LFS PTR (`--dim`,
path in `--muted`).

## Typography

- `--sans`: Space Grotesk 400/500/600/700. Prose, names, headings, buttons that are words.
- `--mono`: Space Mono 400/700. Every measured value, sha, path, count, label, key hint,
  tab, facet, table cell.
- Fonts are loaded from Google Fonts in the prototype; vendor the woff2 files under
  `web/fonts/` and declare `@font-face` locally so the UI works offline.
- Letter-spacing conventions: section labels 10px mono at 0.16em uppercase; tabs and
  buttons 11px mono at 0.1em; column headers 10px mono at 0.13em; state text 10.5px at
  0.08em; wordmark HOARD 14px sans 700 at -0.01em.
- Headings: detail title 21px sans 600 at -0.02em; inspector name 18px sans 600 at
  -0.02em; card h1 29px sans 700 at -0.025em; card h2 17px sans 600 with rule below; card
  paragraph 15px sans, line-height 1.62, max-width 66ch.

## Layout

App shell is `height: 100vh; min-width: 1180px; overflow: hidden`, column flex.

Top bar (`--panel`, 11px 16px padding, 1px `--rule` bottom):
`HOARD` wordmark, nav tabs INDEX / SOURCES (mono 11px, 6px 12px
padding, active has `--rule` border, `--bg` fill, `--amb` text), search field (flex 1,
34px tall, `--bg` fill, border `--rule` or `--amb` when focused, amber glyph, mono 12.5px
input, CLEAR affordance when non-empty, hint `/` or `ESC` at right), node pulse
`4 NODES` in green with a 6px pulsing dot, `LOADOUTS` button in amber outline.

The leading icon in the search field opens the SHELVES list beneath the field's left edge.
Clicking inside the text field only edits the query. The second row is hidden when the query
is empty. When the query is non-empty it shows a `PARSED` label and one chip per token
(`--panel2` fill, `--rule` border, key in `--amb`, value in `--text`, a `x` remover in
`--dim` that turns `--red` on hover), then the result line
`N ARTIFACTS · M NODES` in `--muted`. After the current query has been searched, an
`ADD TO SHELF` button appears beside the chips if no shelf already stores that query. It
opens the naming form beneath the left edge of the search field. The first form row shows
the query; the second has a focused name
field and an add button. Enter also adds the shelf. The icon's list can be searched by shelf
name, opens a saved query on click, and offers removal for user-created shelves. Seeded
shelves have no remove action. Escape and clicking outside close either panel.

Index view grid: `214px minmax(0,1fr) 288px` (rail collapses to `0px` when hidden).

Facet rail (`--panel`, right rule): sections ROLE, SOURCE LAYOUT, MODEL FAMILY,
FORMAT · PRECISION, NODE, each a 10px 0.16em label with a rule below, then rows of 8px
square mark + label + count (mono 12px, 3.5px 4px padding). SOURCE LAYOUT sits second, between
ROLE and MODEL FAMILY: it names the folder discovery filed the entry's repository under,
the placement decision the tree already carries, so a file filed correctly but recognized by
no tensor rule still says where it came from. Active facet: amber mark filled, `--panel2` row.
Count `—` when zero. A row whose label the rail cannot show whole recovers it on hover: as
`label (value)` whenever the two differ, and as the label alone when it is longer than the
22 characters the column fits. A short label that is its own value carries no tooltip,
because repeating it says nothing. MODEL FAMILY prints `27 of 213 — showing the most
common` (mono 11px, `--dim`) under its rows whenever the rail stands for fewer bases than the
result set holds, counted over the bases the rows stand for so a brand row never deflates it.
MODEL FAMILY renders one row per lowercase, numberless family, sorted by count with no
indentation or second tier,
except that `any` sorts last whatever it counts and carries the tooltip `any (works with every
architecture)`. A family row carries its name as both value and label (`qwen`) and has a
tooltip naming its exact indexed bases. It selects their union; its mark is filled only while
every member is selected. Families are grouped before the 48-row limit, so a visible family
includes its less common variants. No section carries a filter field of its own: the
search bar above the rail is the one place a query is typed.
ROLE rows show Checkpoint, ControlNet, Upscaler, Embedding, Dataset, Projector, and
Unclassified in full; LoRA, VAE, and LLM keep their familiar acronyms. Clicking a row still
uses its short search code, and the search bar accepts both short and full names.
DISK PRESSURE shows server-derived reclaimable bytes and duplicate counts; displayed
values come from the current catalog.

Results header (`--panel`): `N ARTIFACTS · M NODES · SORT KEY ↑` at left, view tabs
TABLE / CARDS at right. TREE and DISK views are not implemented.

Table (`min-width: 1010px`), sticky header, nine columns:

| # | Header | Width | Align | Cell |
| --- | --- | --- | --- | --- |
| 1 | ROLE | 76px | left | 3x13px role color bar + 10px role code in `--muted` |
| 2 | NAME | 1fr | left | the row's `prefix` in `--dim` then `display_name` in `--text`, ellipsis |
| 3 | BASE | 100px | left | `--muted` |
| 4 | FORMAT | 96px | left | `--muted` |
| 5 | PREC | 62px | left | `--muted` |
| 6 | SIZE | 76px | right | `--text`, 14px right padding |
| 7 | REVISION | 84px | left | `--muted` |
| 8 | NODE | 104px | left | `--muted` |
| 9 | STATE | 92px | right | 10.5px 0.08em in state color |

Row: `padding: 5px 14px` and 12.5px font when Condensed (28px rows), `10px 14px` and
13.5px when Roomy. `border-left: 2px` shows `--amb` on the cursor row, `--rule` on
selected rows. Click sets cursor, double-click opens. Header click sorts, second click
flips direction, arrow `↑`/`↓` after the label, active header in `--amb`.

Empty state: `NO RESOURCE MATCHES THIS QUERY` and a clickable example query
`compat:sdxl-base-1.0 role:lora`.

Cards view (LoRA identity): grid `repeat(auto-fill, minmax(228px, 1fr))`, 12px gap. Card:
role bar + role code + state at top, name 15px sans 600, `org base · prec · size` in
`--dim`, dashed rule, trigger words line. Cursor card has amber border and
`oklch(0.25 0.015 65)` fill.

Footer key hints (`--panel`, mono 11px): `J/K move`, `↵ open`, `SPC select`,
`L loadout`, `Y yank`, `/ search`, `? keys`, and at right `n selected` or
`double-click a row to open it`.

Inspector rail (288px, `--panel`, left rule, 16px padding, 20px gaps): INSPECTOR label,
role bar + role code, name 18px, path in `--dim` 11.5px; facts list (dashed rows, key in
`--dim`, value right-aligned in `--text`): role, base, format · prec, size, node, layout,
blob sha256, plus rank / alpha for lora, context for llm, resolution for ckpt; buttons
`OPEN REPO ↵` (amber primary), `ADD TO LOADOUT`, `YANK PULL COMMAND` (outline); PLUGS INTO
list of up to four compatible artifacts (`role · name`, amber on hover, click opens).

Model and dataset inspectors also offer an expandable REVIEW CLASSIFICATION section after
the action buttons. It shows the current confidence or why review is needed, the recorded
role/base evidence, and labeled role and model-family fields. CONFIRM CLASSIFICATION stores
the decision through the triage API. Blank family input preserves the current family;
unclassified roles and incomplete sets can remain in triage after confirmation.
CLEAR OVERRIDE appears when a saved decision exists and explains that clearing removes the
decision while current facts remain until the files are re-inferred. REJECT AS NOISE requires
CONFIRM REJECT or CANCEL and explains that future scans skip all indexed paths for the artifact
without deleting files. These controls do not appear for Git projects.

Mutations disable the review form while saving. Failures retain draft values and show an inline
error; owner-access errors link to Sources for token entry. Successful changes refresh the
current search, facets and shelf counts, keeping the selected resource when it still matches.
Keep draft input, focus, text selection and inspector scroll through unrelated rerenders.
Switching to another artifact starts a fresh review. Escape closes the review and focuses its
summary; Enter/Space on summaries retain native behavior. Classification requires the same
node write token as other catalog changes.

## Model paths and identity

The Index retains content-based model identity while displaying the selected loose file's
repository-relative path, including subfolders. Repository model names and shard-set labels
remain canonical. Name sorting uses the same label the row shows. Tooltips expose the full
label without expanding condensed rows; cards and compatibility lists use the same label.
Search matches relative paths, every indexed filename alias, source and repository names.
Matching entries collapse to one artifact; the displayed path is a matching location.

The selected entry travels through inspector reads, repository deep links, copied pull
commands and loadout additions. An explicitly missing or mismatched entry is refused rather
than silently selecting another copy. Older requests without a location use the preferred copy.
New collection destinations preserve subfolders under known model categories. Saved paths
remain explicit and unchanged. Recognized ComfyUI/A1111 category repositories stay stable
when all their weights are nested in subfolders.

Verification covers nested-only categories, same-name models in separate folders, renamed
duplicate content, filtered-source actions, selected-entry validation, sorting/pagination,
saved-path preservation and upgrading an existing derived search index without a rescan.

## Sources view

A bare initial URL opens Sources when `/api/v1/setup` confirms zero configured folders and
zero resources. Existing nodes keep Index as their landing view; explicit queries and shared
detail links are preserved. There is no permanent onboarding-complete flag. The compact
`Build your shared catalog` section appears while no resources are indexed, with Add a folder,
Review resources and Share access guidance. It reuses the shell and existing add-folder form.
Keep configured-empty, scanning, failed scan and unmatched-search states distinct. Empty
Index results offer CLEAR SEARCH and/or an actionable Sources link instead of an irrelevant
model query example. Background scans refresh an empty Index until resources appear.

SHARING & SETUP remains a collapsible section in Sources after resources appear. It shows
the active address/listener, node name/address fields and honest local-only or unverified
sharing status. Saving an address updates the active address and generated commands immediately;
only a changed node name shows a restart requirement. Explain that the sharing address is used
in copied links and commands. Do not silently restart or rebind. Token-protected nodes offer
password input for owner
access, held only in tab memory; explain that reads remain public. Preserve unsaved form
input, focus and expanded state through polling. Paths in the add-folder form explicitly
refer to the machine running Hoard, with files left in place.

One SOURCES tab combines folders, nodes, and jobs. Its header summarizes their counts and
offers ADD FOLDER. The form takes an absolute path on this node, an editable name, and a layout
choice defaulting to auto. Adding persists the folder, queues its first scan, and registers it
with the watcher while the server runs. Configured folders appear before their first scan.

FOLDERS ON THIS NODE lists path, layout, file count, size, last scan, status, and SCAN. OTHER
LOCAL STORAGE holds download and pull sources, plus catalog rows no longer configured. NODES
shows this node first, then peers with online state, address, indexed size, and last seen.
ACTIVITY shows running jobs with their subject, phase, progress, rate, and cancel action;
recent completed jobs are under a collapsible heading. These sections refresh while visible.

Each folder's existing action column holds BROWSE / SCAN / REMOVE. BROWSE opens a `source:`
query in Index, for models or Git projects alike; no second result section appears in Sources.
REMOVE arms CONFIRM / CANCEL in the same cell and the status reads `Files stay on disk`.
While removal runs the controls are disabled and status reads `Removing...`. The row vanishes
on success; other folders retain their scan state. Armed removal and keyboard focus survive
polling. Escape cancels an armed removal. The add-folder form and its focus survive polling.

Removal is persistent for UI and TOML sources and does not delete disk files. A new add of the
same name restores it. Finished scan jobs have DISMISS; RECENT JOBS offers CLEAR FINISHED
SCANS. Active scans and non-scan jobs are protected. History cleanup never removes sources.

## Git-project resources

Git projects occupy normal Index rows/cards, with one scroll container and the existing
columns, cursor, inspector, sort controls and pagination. ROLE reads GIT, FORMAT reads git,
BASE and PREC are absent. The rows are globally sorted with artifacts. Click selects;
double-click or Enter opens; J/K moves through either kind; Space selects; Y copies a pull
command for an artifact or a clone command for a project. The project inspector shows project
facts and OPEN PROJECT / COPY CLONE, without loadout or compatibility actions. L cannot pin a
project or a stale model selection. TABLE / CARDS changes both resource kinds together.

A project is findable by name, committed README text and path without any model artifacts.
Existing ROLE and FORMAT facets include Git project / git; `role:git` and `format:git` are
presentation filters, not inferred model roles. Shared node, source, repo, org, revision,
size and state queries include projects. Model-specific base, precision, layout and compat
filters remain model-specific. Shelf counts describe the resources their queries display.

`#/project/{id}` uses the established detail header (21px title), tab strip, revision bar,
content panel and 340px right rail. README opens first; FILES and REFS use the existing compact
row styles. SWITCH opens REFS; clicking a ref changes the viewed commit. The rail holds the
clone command, COPY CLONE / COPY PROJECT LINK and project facts. There is no separate tall
form header or model-specific compatibility/loadout control. Brackets cycle README / FILES /
REFS. Y copies the clone command.

FILES has the standard compact file rows. Selecting a file drills into a full-width escaped
preview or explicit binary/oversized message; FILES or Escape returns to the remembered list
position. DOWNLOAD FILE retrieves a committed file up to 16 MiB; larger files remain available
by cloning. Previews stop at 256 KiB. Relative README documentation links open the file view.
Missing README, missing revision and unavailable files have explicit states and retry paths.
Escape from the project and the header back button return to the remembered search, including
loaded pages, cursor and scroll. Shared links retain revision and selected file.

## Repo detail view

Header (`--panel`, 16px 18px): `← ESC` button, role color bar 3x22, `org/name` 21px
sans 600 (org in `--dim`), role badge (CHECKPOINT, LORA, CONTROLNET, UPSCALER, VAE,
EMBEDDING, LANGUAGE MODEL, DATASET, UNCLASSIFIED), state badge
(`SERVED · hoard.lan`, `PULLING 64%`, ...), meta `path · size · last used 2h ago` in
`--dim`, `+ LOADOUT` button.

Body grid `minmax(0,1fr) 340px`.

Tabs FILES / REFS / CARD / COMPAT / USAGE (mono 11px, active has `--rule` border with the
bottom border erased into `--bg`, `--amb` text). `[` and `]` cycle tabs.

Revision bar (`--panel`): REVISION label, `refs/heads/main` chip in `--panel2`,
`462c3cb · 14 files · 3 branches · 2 tags` in `--dim`, `SWITCH ▾` in amber at right
(opens REFS).

FILES: grid `minmax(0,1fr) 96px 104px 88px`: path, kind (config, weights, single-file,
card, git, `9 files` for collapsed groups), size right, state right. 7px 18px padding,
12.5px mono.

REFS: grid `22px minmax(0,1fr) 110px 150px 84px`: dot (`●` branch, `⊢` tag; amber when
current), name, short sha in `--amb`, relative time in `--dim`, kind right. Click checks
out that ref for the detail view and toasts `checked out v3 · a1f9e02`. Footer note: refs
are read straight from the clone; pinning in a loadout stores the resolved sha.

CARD: centered paper panel `max-width: 760px`, `--panel` fill, `--rule` border. Title bar
(`--panel2`): amber square glyph, `README.md`, `· <rev>`, `4.1 KB · 118 lines` at right.
Body 26px 30px padding. Optional YAML FRONTMATTER box (dashed border, mono 11.5px, key in
`--amb`). Then blocks: h1, h2, p, li (amber middot in an 18px column), code (`--bg` fill,
mono 12px, pre-wrap), quote (2px amber left border, `--muted`). When no README exists the
page is generated from the index record: "No upstream card was recovered..." with What we
know (role, base, trigger words) and Pull it (the working command, `git clone` only when the
repository can be cloned).

COMPAT: banner `RESOLVED FROM TENSOR KEYS AND RANK — NOT FROM THE MODEL CARD ·
<N host checkpoints accept X | N adapters resolve against X>`; rows grid
`54px minmax(0,1fr) 110px 96px 92px`: role, name, detail (`rank 32 · w 0.4` for lora,
else precision), size right, state right. Click opens that artifact.

USAGE: LAN ACTIVITY — LAST 7 DAYS bar chart (14 bars, 74px tall, bars above 70% in
amber, others alternate `--panel2` / `--rule`), footer `aug 28` and `38 pulls · 3 clients`;
RECENT PULLS grid `130px 150px minmax(0,1fr) 100px`: when, who (client host), what
(path), rate in `--green`.

Right rail (340px, `--panel`): PULL TO THIS MACHINE code box, whose git block
(`# git, refs preserved`, `git clone http://hoard.lan/org/name`) is shown only for a
repository this node can serve over the git face, followed by `# or python, unchanged`
(`# python, unchanged` when no git line precedes it) and
`HF_ENDPOINT=http://hoard.lan:7099` with the URL in amber. That line names the artifact the
page was opened on; a `# whole repository` caption and the repository-level line follow it
where the repository is one whose whole contents are worth pulling, and a category folder of
a tool layout shows the one line alone. The COPY button copies the first
line shown. DERIVED
FACTS list (same shape as inspector facts, plus `first seen`, `pulls on lan`); PATHS ON
DISK list (`path · node`, symlinks shown as `laptop · symlink`).

## Loadout collections

Loadouts are named collections of arbitrary models for selective downloads to the client.
Their complete interaction and acceptance contract is in [Model collections and client
 downloads](loadouts.md). The existing drawer and Index renderers remain the shared surfaces.

Collections appear as LOADOUT rows in mixed Index search and `role:loadout` results, with
member count and selected byte size. Searching also matches member filenames. A model's
inspector lists RELATED LOADOUTS. Opening either a collection row or a related link shows
its stable drawer; `#/loadout/<id>` supports reload and sharing.

The toolbar LOADOUTS button opens a chooser. ADD TO LOADOUT or L opens that chooser for the
current or selected models; choose a collection and confirm ADD, or CREATE
one with a name. Models append without role-slot replacement. RENAME reveals a SAVE/CANCEL editor. Each model defaults to one condensed disclosure row;
its filename and size stay visible, while source details, SAVE PATH and REMOVE expand on
click or keyboard activation. Full filename and destination are available in its tooltip.
The picker and NEW share one row; count, bytes, RENAME and DELETE share the management strip.
Form inputs and buttons align at equal heights. DELETE requires a named confirmation and
preserves original files. CRUD updates patch the mounted drawer rather than recreating it
or replaying its animation. Preserve drafts, focus, selection and body scroll.

The compact download footer shows a client destination root, script format, COPY SCRIPT and
SAVE SCRIPT. Folder layout, help and PowerShell/POSIX script previews expand on demand. Scripts request
only the selected files through `hf`, preserve existing client files with a warning, and
stop on transfer failure. The UI reports script readiness, never download completion.
Unavailable members, destination collisions and unsupported multi-file shard artifacts
block script preparation. A path may be edited independently of the model's classification.
No server-side Apply, tool-layout selector, projection root or target-node control appears
in this collection journey. Generic server projection APIs remain separate for existing
callers. Existing legacy loadout documents remain readable; the catalog format is unchanged.

Toast: bottom center, `--panel2` fill, 3px amber left border, mono 12px, 2.4 s, used for
`Loadout created`, `Models added to loadout`, `Loadout name saved`,
`Download path saved`, `Loadout deleted; model files kept`, `copied: git clone ...`,
`served name → http://hoard.lan/org/name`, `checked out ...`,
`apply complete on laptop` only after the job succeeds, or its failure/cancellation.
Collection notifications describe saved membership or prepared client scripts; they never
claim that a download finished.

## Keyboard map

Global: `Esc` blurs the search field, else closes the drawer, else leaves detail.
`/` focuses search (prevent default). Keys are ignored while typing in an input.

Index: `j`/`↓` and `k`/`↑` move the cursor (clamped), `Enter` opens, `Space` toggles
selection, `L` opens the collection chooser for the cursor model or selected models,
`Y` yanks the pull command (or opens a collection's download controls), `?` shows the key sheet.

Detail: `[` and `]` cycle FILES, REFS, CARD, COMPAT, USAGE; `L` opens the collection chooser for the opened file and adds it to
a chosen collection; `Esc` returns to the index.

Drawer: Tab reaches the collection selector, NEW and management actions. RENAME opens an
explicit SAVE/CANCEL editor. Enter or Space on a member summary expands or collapses its
source, editable destination and REMOVE action. DELETE opens a named confirmation; CANCEL
returns focus to its invoking control. Background updates preserve expanded rows and drafts.

Mouse: row click sets cursor, double-click opens, facet click toggles its token in the
query, shelf click replaces the query, column header click sorts, tab click switches.

## Query grammar

The [Hoard search API](../reference/api/hoard.md#search) owns grammar, aliases and matching
semantics. The UI renders the server's parsed tokens, warnings and facet/shelf counts; it
never implements another query parser. Free text includes model paths and filename aliases,
while Git projects also search their indexed file paths and committed documentation.

Facet counts exclude their own active key, and clicks use the server-provided token/query.
Saved shelves use the same grammar and count distinct resources. Built-in shelves come from
the catalog; user-created shelves can be added and removed through the existing controls.

## Compatibility rule

```
hosts_of = { lora: {ckpt, llm}, ctrl: {ckpt}, vae: {ckpt}, upsc: {ckpt},
             emb: {ckpt, llm}, proj: {llm} }
compatible(host, other):
  false if same artifact or either is a dataset
  an adapter accepts a host whose role its own hosts_of names, and then
    proj: the two are in the same repository, whatever either base says
    every other adapter: other.base == host.base or either base is "any"
```

## Props exposed by the prototype

`density` Condensed | Roomy, `roleColorMode` Color-coded | Monochrome, `showInspector`
boolean. These settings are persisted in `localStorage`.

## Additional application behavior

The key sheet, usage/trigger queries, disk-pressure summary, Git serving, filesystem
projection and peer discovery are implemented behavior beyond the original visual prototype.
TREE and DISK views are not implemented. See [product scope](product-scope.md) and the
[API reference](../reference/api/hoard.md) for supported behavior.
