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
`4 NODES` in green with a 6px pulsing dot, `LOADOUT n` button in amber outline.

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
The proposal also shows a DISK PRESSURE mini bar chart with
`311 GB reclaimable — 47 duplicate blobs`.

Results header (`--panel`): `N ARTIFACTS · M NODES · SORT KEY ↑` at left, view tabs
TABLE / CARDS (TREE and DISK are listed in the proposal, not built) at right.

Table (`min-width: 1010px`), sticky header, nine columns:

| # | Header | Width | Align | Cell |
| --- | --- | --- | --- | --- |
| 1 | ROLE | 54px | left | 3x13px role color bar + 10px role code in `--muted` |
| 2 | NAME | 1fr | left | the row's `prefix` in `--dim` then name in `--text`, ellipsis |
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
sharing status. Saved settings show a restart requirement without changing active labels.
Do not silently restart or rebind. Token-protected nodes offer password input for owner
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

## Loadout drawer

Right-anchored 520px panel over a scrim, `hoardIn` animation. Header: `loadout` 19px sans
700, then the current loadout's name in an amber mono field, borderless until it is focused,
and `none` as a plain amber slug when the node holds no loadout; CLOSE button. Enter in that
field, or leaving it, sends the name as typed and the server refuses a blank one. Rows grid
`52px minmax(0,1fr) 78px 60px 22px`: role, name, rev in `--dim`, weight in `--amb`
(lora `0.55`, ctrl `0.70`, upsc `2.0×`, others `—`), remove `×`. Below the rows a `LOADOUTS`
block lists every loadout on the node in a grid `minmax(0,1fr) 64px 72px`: the name (amber on
the current one, click switches to it), `n items` dimmed, and `DELETE`, which reads `CONFIRM`
in amber after one click and removes the loadout on the second. Below that a NEW form: a
field placeheld `name, or blank for loadout-N` and a NEW button; Enter in the field does the
same. Empty states: `EMPTY LOADOUT <name> / a checkpoint and the adapters you use with it /
select artifacts in the index and press L, or start another with NEW below` when a loadout is
current, and `NO LOADOUT YET / a checkpoint and the adapters you use with it / press L on a
row to start one, or NEW below` when none is; the middle line says what a loadout is for.
`L` with no loadout creates `loadout-1` and pins into it; `L` on a checkpoint while the
current loadout has one starts a loadout named after the file and pins it there. The current
loadout is remembered in the settings record. Footer: terminal box
`$ hoard apply night-exterior-v3 --node laptop` with result line in green
(`→ 3 present, 2 pulling over LAN (1.2 GB/s)` then
`→ done · 5 artifacts resolved, symlinks written for comfyui`), and an amber
`APPLY TO LAPTOP` button.

Toast: bottom center, `--panel2` fill, 3px amber left border, mono 12px, 2.4 s, used for
`+ name pinned at v3`, `replaced vae X with Y`, `new loadout "X": checkpoint pinned`,
`new loadout "X"`, `renamed to X`, `removed loadout X`, `copied: git clone ...`,
`served name → http://hoard.lan/org/name`, `checked out ...`,
`applied to laptop · symlinks written for comfyui`. Every pin toast is the server's own
`notice`, so the drawer decides nothing about what a pin did.

## Keyboard map

Global: `Esc` blurs the search field, else closes the drawer, else leaves detail.
`/` focuses search (prevent default). Keys are ignored while typing in an input.

Index: `j`/`↓` and `k`/`↑` move the cursor (clamped), `Enter` opens, `Space` toggles
selection, `L` pins the cursor row into the current loadout and a second checkpoint starts a
loadout named after it, `Y` yanks the pull command, `?` shows the key sheet (proposal
footer).

Detail: `[` and `]` cycle FILES, REFS, CARD, COMPAT, USAGE; `L` pins the opened file into
the current loadout; `Esc` returns to the index.

Drawer: the header name field renames on Enter or blur; NEW starts a loadout named as typed
or `loadout-N`; a loadout row switches on click; DELETE arms, CONFIRM removes; the row's `×`
removes a line.

Mouse: row click sets cursor, double-click opens, facet click toggles its token in the
query, shelf click replaces the query, column header click sorts, tab click switches.

## Query grammar

Tokenizer: split on whitespace, quoted strings kept whole (`trigger:"film grain"`).
`key:value` when a colon is present, otherwise free text. Leading `-` negates a token.
Leading `~` forces fuzzy matching over everything indexed. Comma inside a value is OR.

| Key | Semantics |
| --- | --- |
| `role:` | alias table: checkpoint/ckpt/base/model/complete/cmp/backbone/bb, lora/lycoris/locon/adapter/ip-adapter/ipa, controlnet/ctrl/control/motion, upscaler/upsc/upscale/esrgan, vae, embedding/emb/textualinversion, llm/language model/text, dataset/ds/data, proj/projector/mmproj/vision-projector, ?/unknown/unclassified/triage |
| `base:` | architecture family, compared with spaces and dots stripped (`sdxl`, `sd15`, `flux`, `wan`, `llama`, `qwen`) |
| `fmt:` / `format:` | container: safetensors, gguf, diffusers, pth, pt, parquet, jsonl.gz |
| `prec:` / `precision:` | fp16, bf16, fp32, fp8, int8, nvfp4, nf4, q4_k_m, q8_0, ... |
| `node:` | which machine holds the bytes |
| `layout:` | the ComfyUI or A1111 folder the entry's repository was filed under (`checkpoints`, `diffusion_models`, `text_encoders`, `loras`, `controlnet`, `vae`, `upscale_models`, ...), or `hub repo`, `git clone`, `(unplaced)`; folds a folder spelling onto its canonical layout the way `role:` folds its aliases |
| `state:` | served, local, pulling, stale, remote, triage |
| `rev:` / `tag:` | revision, branch or tag name |
| `dupe:true` | blobs whose bytes exist as more than one file |
| `compat:<repo>` | directional compatibility against that repo |
| `size:<400mb` / `size:>10gb` | comparison with kb/mb/gb/tb units, default mb |
| `used:>30d` | last access age (proposal) |
| `trigger:"phrase"` | exact phrase in trigger words or model card (proposal) |
| free text | word-prefix match over name, org, base, trigger words, tensor keys and the card; `~` for fuzzy |

Facets compute their counts with their own key removed from the query so a click never
produces zero. Shelves in the prototype: `triage queue` = `role:?`, `sdxl · lora · small` =
`role:lora base:sdxl size:<400mb`, `flux workhorse` = `base:flux`, `quantised llm` =
`role:llm fmt:gguf`, `cold + duplicate` = `dupe:true`, `everything on laptop` =
`node:laptop`, in that order.

The SHELVES dropdown lists them in the same order, with name, query, and count. COUNT is
the shelf's own count, shown as `—` when the shelf was not counted. Only a user-created
shelf carries the remove button.

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
