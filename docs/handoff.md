# Handoff

The state of the repository, how work is run on it, and what is left. Read `CLAUDE.md`
first; this document assumes it.

## Latest delivery: first-run setup and repeatable introduction

An empty installation needs no TOML file or example folders. The bare URL opens Sources
only when the server confirms no configured sources and no resources, including peers.
Explicit queries and shared details retain their routes; detail loading does not wait for
setup status. The short Sources guide uses the accepted shell, existing add-folder form
and normal Index. It disappears after resources arrive, deferring removal while a folder
form is being edited to avoid layout shifts. Empty searches, empty folders and background
scanning have distinct guidance. Removing the last source makes setup useful again.

SHARING & SETUP persists validated node name/address overrides in `node.json`. Active and
pending values remain distinct until restart; saving neither restarts nor rebinds. Local
command URLs use loopback rather than a wildcard, while LAN discovery preserves its existing
hostname behavior. The browser can hold an owner write token in tab memory, cleared on
reload. Reads/downloads stay public to reachable clients; the write token is not read access
control. See [first-run guidance](first-run.md) and the [API contract](api/hoard-api.md).

The [narrated introduction](introduction-video.md) uses real UI actions, synthetic Git/model
fixtures, a verified HTTP clone, captions and offline narration. Its checked-in recorder and
editable storyboard reproduce the journey from an empty temporary catalog through sharing
configuration, restart, indexing, search and review. Media validation checks complete decode,
audio/video durations, audible unclipped narration and reading pauses. Source fingerprints
and a dirty-checkout declaration distinguish recording evidence from an exact clean-commit
claim. No production configuration, catalog or service is part of the recording.

The original libflite voice was rejected. The current video instead uses the owner's
supplied `docs/media/narration/hoard-introduction.wav`, with its matching narration text.
The recorder aligns chapters to the ten paragraph pauses, preserves continuous audio at
its original speed, and records the source audio hash and boundaries. It does not call an
audio generation service; the supplied file suffices for future reshoots. Captions and
transcript follow the new narration. Focused timing tests and a complete candidate reshoot
verify this replacement independently of the earlier application test results below.

`node scripts/verify-first-run.mjs` separately checks fresh arrival, explicit empty queries,
owner access, invalid-path recovery, empty scans, polling focus, form position during a
background scan, active/pending settings, restart, deep-link reload with setup unavailable,
token storage and removal of the last source. Fresh screenshots cover both themes at
1180/1600px; the existing 32-state resource/layout journey also passes. Keep these broader
checks separate from the promotional recording's narrower assertions.

Final checks: 1,314 workspace tests passed, 0 failed and 11 pre-existing tests ignored;
formatting, all-target Clippy with warnings denied, workspace build, six release asset tests
and documentation build passed without warnings. First-run browser acceptance and the
existing 32-state regression journey passed. The complete narrated reshoot passed its
16 assertions, including actual cloning, stable source fingerprints, complete decode,
matched audio/video duration and narration levels. The UX agent accepted the final flow
and recording frames. Production, port 8080 and live catalogs were untouched.

## Accepted design baseline and lessons

The owner confirmed that `82bc336` behaves as expected within the application design.
The durable [UI change workflow](design/README.md#ui-change-workflow) records the lessons:
reuse existing surfaces, establish before/after evidence, review existing resource types,
and verify polling, navigation and removal lifecycle alongside the new feature journey.
`CLAUDE.md` and the web rules point future implementers to that workflow. The current
specification now explicitly distinguishes accepted changes from historical prototypes.

## Latest delivery: cohesive resource browsing and removable Sources

The first Git delivery passed a narrow acquisition journey but introduced a separate results
block and oversized project header. The owner rejected the Index and Sources layout changes.
This correction restores the established surfaces: artifact and Git rows/cards share one
cursor, inspector, scrollbar, sort and pagination; project detail reuses the existing header,
tabs, revision bar and right rail. Sources BROWSE navigates to a normal source query instead
of inserting another browser. Model-only inspection and loadouts retain their established
behavior; projects have matching neutral actions and facts. Shelf counts include resources.

Sources now removes folders with inline REMOVE / CONFIRM / CANCEL, preserving the original
files. `sources.disabled.json` persists exclusions alongside `sources.json`; TOML is untouched.
UI restoration by name overrides that TOML source without rewriting it. Startup completes
persisted removals before watchers and requests start. The coordinator serializes removal
with scan execution/admission, prunes queued source requests and their displayed subjects,
retires only the target source through the existing bounded catalog path and refreshes
search. Owned tasks retain guards
even after a caller disconnects. Other sources and shared artifacts remain intact.

Finished scan history can be dismissed or cleared separately. Queued/running scan IDs stay
protected even when their job row is already marked cancelled. The admission gate covers
history cleanup and the active-ID snapshot; an internal `jobs.high_water` Setting keeps IDs
monotonic after cleanup and reopen, including old catalogs without that key. Stored catalog
shapes and `FORMAT_VERSION` remain unchanged.

The broad browser review compares artifact-only baselines at 1600/1180px across light/dark,
both densities, inspector on/off and TABLE/CARDS (32 combinations), then exercises mixed
resource keyboard/cursor/inspector, standard project detail/refs/files, Sources browsing,
form/confirmation focus through polling, UI/TOML removal, preserved files, watcher exclusion,
restart, re-add and HTTP cloning. Async removal/navigation and loaded-page return paths are
also checked. Production, port 8080 and live catalogs are not used for verification.

Final verification on 2026-10-31: formatting and all-target Clippy (`-D warnings`) pass;
workspace build passes; workspace tests report 1,304 passed, 0 failed and 11 existing ignored
tests (scale, memory, local-cache and multicast fixtures). No ignored test was introduced.
All six release asset tests pass, and the documentation build completes without warnings.
The independent UX review accepts the corrected design.
The isolated final browser pass covers all 32 display combinations plus resource pagination
and return scroll, saved-shelf counts, removal during navigation, polling focus, preserved
files, restart/re-add and actual HTTP cloning, with no browser exceptions.

## Previous delivery: find, review and clone Git projects

Git projects are now resources in the main Index and Sources rather than an incidental Hub
repository detail page. Main search returns `projects` beside the artifact rows; it matches
project names, source/reference, committed README text and file paths. The derived in-memory
Tantivy index lives behind the server state, is built lazily, and refreshes when the scan lane
finishes. It adds no stored record or catalog format bump (`FORMAT_VERSION` remains 19).

`web/js/projects.js` renders neutral `#/project/{id}` pages: README, file browser, escaped text
preview, revision selector and clone/share controls. The new `/api/v1/projects` route family
reads committed blobs through `GitRepo::committed_blob`, including bare repos, and never reads
dirty worktree content. Preview and download bounds are 256 KiB and 16 MiB respectively.
The canonical `/git/projects/{id}.git` face supports any indexed Git repository without
implying it is a model or dataset. The old artifact repo URL builders now agree on `-` for
an absent org. Markdown rendering disables unsafe link and image schemes.

The user-agent accepted the implemented journey. An isolated headless browser exercised
search, README, relative documentation links, nested/binary/oversized files, long-list scroll,
revision changes, shared-link reload, Sources navigation and actual HTTP cloning. Durable API
tests verify committed content, bare repos, branch/tag cloning and search refresh after a
completed source scan. Every fixture uses temporary data and ephemeral loopback ports.

Validation passed: workspace tests, all-target Clippy with warnings denied, formatting,
workspace build, all six release asset tests and the documentation build without warnings.
The final browser pass also covered legacy repository-link redirection and delayed search
responses. Review fixes preserve case-sensitive source drilldowns, reuse artifact word and
phrase rules, bound aggregate README text, stream project documents and release the cache
lock before concurrent searches. Project share links use the configured public URL.

Production services, port 8080 and the live catalog were left untouched. Deploying for LAN
clients still requires a reachable bind address and `public_url`; this task did not change the
running node's loopback configuration. Folder removal and completed-scan cleanup were
subsequently delivered in the cohesion correction above.

## Earlier review: Sources and repository discovery (2026-10-31)

The redesign through `ce1c098` is implemented: preferences replace the top-bar theme control,
the rail groups model families and spells out roles, shelves live in the search controls,
and Sources combines folders, nodes and job activity. UI-added folders persist in
`<data_dir>/sources.json`, load alongside TOML sources and join the running scan lane and
watcher. The README and UI specification now describe that delivery.

Three gaps remain: there is no source-removal endpoint or button, completed scan jobs cannot
be cleared, and no UI consumes `GET /api/v1/repos`. The owner requested both folder removal
and completed-scan cleanup. A Git repository without model artifacts is therefore invisible
in the Index even when successfully scanned. Read-only checks found the reported `pose-character`
source and repository, and `git ls-remote http://127.0.0.1:7099/pose-character.git HEAD`
succeeded. Its Git clone URL works independently of artifact search.
Its existing detail page is reachable at `#/repo/models/-/pose-character`. The UI's orgless
URL builder instead emits or omits an empty segment, which fails the API's three-segment
route; fix that when adding repository navigation.

[Design status and next work](design/README.md) maps the historical plans to landed commits
and records the next implementation: persistent source removal coordinated with scans and
watchers, completed-scan cleanup, then repository browsing and server-generated clone commands
from Sources. Removing a source must leave its disk files intact. Clearing completed jobs
only removes their history and must refuse active jobs.

Production occupies port 8080 and the machine is shared with other work. This review did not
restart services or mutate the running catalog. Future verification must use temporary data,
an available loopback port and limited build/test/hash concurrency.

## Latest wave: several named loadouts

A node now holds any number of loadouts and no pin replaces a line silently. No stored record
changed shape, so `catalog::FORMAT_VERSION` stays 19: `LoadoutDoc` already keyed on a unique
name and already had a remove path, and the wave is rules in core, two catalog writers, three
route changes and the drawer.

The core rules. `hoard-core::loadout::Loadout::add` answers `Placed { slot, replaced }`, its
host, VAE and upscaler arms using `Option::replace` so the displaced pin comes back, and
`Loadout::holds_path` answers "already pinned" without a write. `loadout_stem` drops a final
dot followed only by ASCII letters, so `cyberrealisticPony_v160.safetensors` names a loadout
`cyberrealisticPony_v160` while `sdxl-base-1.0` and `Meta-Llama-3.1-8B` are unchanged;
`free_loadout_name` counts `-2`, `-3` past every taken name and `numbered_loadout_name` hands
out `loadout-1`, `loadout-2`.

The catalog. `catalog/loadouts.rs::create_loadout` reads the unique `name` secondary key
inside its own `rw_transaction` and refuses a taken name with
`IndexError::Domain(HoardError::InvalidLoadout)` rather than merging, then `insert`s;
`rename_loadout` refuses a name another row holds, rewrites the name inside the document as
well as on the row, and accepts the loadout's own current name. `upsert_loadout` is untouched
because `hoard loadout import` replaces by name on purpose.

The routes. `POST /api/v1/loadouts` takes `{}` or a blank name and numbers it;
`PUT /api/v1/loadouts/{id}` renames; `DELETE /api/v1/loadouts/{id}` answers
`{ "id", "deleted" }`; a taken name is 409 on every one of them.
`POST .../items` takes `on_second_host: "replace" | "branch"` and answers `PinnedView`: the
loadout view of whichever loadout holds the line, `replaced` and the `notice` the drawer
prints. `api/loadouts.rs::pin_into` answers a path the loadout already holds without a write,
branches a second host into a loadout named after the file, and otherwise adds and reports
what it displaced. `unpinnable` became `loadout_conflict` and the shared
`blocking_or_conflict` carries the three writers that must answer 409.

The drawer. `web/js/loadout.js::loadLoadout` lists every loadout, adopts the one the settings
record remembers or the first one, and falls back without an error when the stored id is
gone; `renderDrawer` prints the header name field, the LOADOUTS list with `n items`, DELETE
then CONFIRM, and the NEW form. `web/js/app.js` gained `switchLoadout`, `newLoadout`,
`renameLoadout`, `armDelete` and `deleteLoadout`, and `addToLoadout` now prints the server's
`notice` and follows the id the server answers. The JS decides nothing: the "already in the
loadout" check and the `revision` and `name` the repo page used to pass are gone, which is
why the repo page's toast names the file rather than the repository.

Live readings, taken at `940bee8` on a scratch catalog of the owner's disk at
`127.0.0.1:7199`, offline, with no lookup: (1) `POST /loadouts {}` answered `loadout-1`; (2)
`+ anima_context_detailer_base10.safetensors pinned at main`; (3) the checkpoint landed
beside it as `["host", "adapter"]`; (4) a second checkpoint with `branch` answered a new id
`ld_2` named `animaOfficial_preview2`, one item, notice
`new loadout "animaOfficial_preview2": checkpoint pinned`, and `loadout-1` still held its 2
items; (5) a second LoRA into the new loadout made 2 items there; (6) the listing read
`[{"animaOfficial_preview2", 2}, {"loadout-1", 2}]`; (7) `PUT` renamed it to `night-test`,
which the document carries too; (8) two VAEs exist on this disk, and the second answered
`replaced vae ae.safetensors with flux2-vae.safetensor` with
`replaced: {"slot": "vae", "name": "ae.safetensors"}`; (9) `DELETE` answered
`{"id": "ld_1", "deleted": true}` and left one row. In the browser at 1720x1000, `L` on a LoRA
gave `LOADOUT 1`, a checkpoint `LOADOUT 2`, a second checkpoint the branch toast and
`LOADOUT 1`; the drawer listed both loadouts with their counts, switching repainted the items
and the count, a rename in the header survived a reload, DELETE read CONFIRM before it
removed, and the last delete left `NO LOADOUT YET / a checkpoint and the adapters you use with
it / press L on a row to start one, or NEW below`. On a repository page whose heading reads
`comfy/diffusion_models`, `+ LOADOUT` toasted
`+ anima_aestheticV11.safetensors pinned at main`.

A line is identified by its repository and path together: `Loadout::holds_line` and
`Loadout::remove_line` take a `RepoId` beside the path, so the same file name under two
repositories is two lines, both pin, and a remove takes only the line the named artifact sits
in. `CreateBody::Unnamed` carries a `deny_unknown_fields` `NoBody`, so a mistyped or
misspelled create body is refused rather than read as "number it". The drawer sends the name
field as typed and lets the server refuse a blank one.

One test changed an assertion: `adding_routes_by_role_and_refuses_datasets` reads `.slot` off
the `Placed` that `add` now answers, and its `remove_path` call became `remove_line` with the
pin's repository. Nothing else was rewritten.

What is left: an apply resolves its loadout by id at plan time, so deleting a loadout while
its apply runs fails the job at that read; `hoard loadout import` still replaces a same-named
loadout, which its clap help now says; the drawer has no weight editor and no key for
switching loadouts; the free-name numbering in `create` and `branch_into` reads
`list_loadouts` in one transaction and creates in another, so two concurrent creates make the
loser a 409 rather than the next number, which one owner driving one UI does not hit and which
no retry loop pays for today; and `armDelete` re-renders the drawer, so a half-typed NEW name
or rename is lost when DELETE is armed.

## Latest wave: a card by whole words, a stored declared total, one triage predicate

Five things the walk of 2026-09-18 found wrong on the owner's own catalog, and the record
changes behind them. `catalog::FORMAT_VERSION` is 19 and `LOOKUP_MEMO_VERSION` is 2: the
catalog is deleted and rebuilt from disk, and every hash the lookup memo held is asked about
once more.

A card is a field of its own. `search/schema.rs` adds `Fields::card`, written with the same
`hoard_words` tokenizer as `text`, and `document()` writes the revision's card there instead
of into `text`. `query.rs::text_filter` now takes the whole `QueryFields` and asks both: a
word is matched from the start of a word of the name, the org, the base, the trigger words and
the tensor keys, and whole against the card, a phrase being a `PhrasePrefixQuery` over the
first and a `PhraseQuery` over the second, with either side answering the row. That is what
stops `wan` finding a card that says `want` and `2 2` finding `42.2`.

The record stores what a set declares. `ArtifactRecord::declared_total` is
`ShardSet::declared_total` carried through `GroupFacts` and `RecordFacts`, `SearchRow` and
`RowView` carry it as `shard_total`, the triage reason reads `1 of 15 shards present,
incomplete` and the inspector's `files` fact reads `1 of 15 shards, incomplete`. Nothing in the
mirror's ingest or pull path fills it: neither writer joins a set's record, so a scan remains
the only thing that turns a set complete. Beside it, `hashing.rs::rekey_artifact` now writes a
set record's `artifact.sha256` when the digest of its anchor arrives, keeping the identity the
set was filed under, which is what left two loose sets on this workstation with
`blob_sha256: null` while every entry under them carried a hash.

One triage predicate. `ArtifactRecord::is_queued` and the `triage_cleared` field are gone;
the queue, the summary and the entry state a scan writes all read `needs_triage`, and the
summary counts it only where `path_count > 0`, so a row only a peer's digest created asks this
node nothing. `Verdict::Restored` and `enter_triage` are the mirror of the rejection path:
`Noise` carries the artifact it was rejected from, and `Catalog::remove_noise` clears the
rejection when no other noise row names that artifact, so a path taken off the noise list puts
its artifact back in the queue, the shelf and the total at once instead of waiting for a scan.

The lookup keeps what it was told. `Listed` carries `trained_words`, `ArtifactDetail` carries
the `listing`, and `apply_lookups` writes a `base_label` only for a spelling hoard's own rules
do not produce, so one key is never printed two ways; the eleven `LISTED_BASES` rows whose key
the rules name (`sdxl`, `wan`, `ltx`, `krea2`, `anima`, `ernie`, `qwenimage`, `mageflow`,
`minimaxh3`, `hidreamo1`, `ideogram4`) spell their label as the rules do, while the lineages
keep the label `Base::LINEAGES` gives them and the variants keep Civitai's spelling. An
answer's `trainedWords` become the artifact's trigger words when nothing stronger than a
file-name or front-matter guess is stored, never over `ss_tag_frequency`.

The server prints it. `common::facts` says `1 of 15 shards, incomplete` for a short set and
`common::listing_fact` adds the `civitai` line; `api/artifacts.rs` and `repos.rs::repo_facts`
both append it, a generated card is headed by the file it describes and prints the listing
under "What we know", and `LoadoutItem` carries `abs_path` so the comfy export links the path
this node holds and writes `# <name>: not on this node` for a line nothing here holds.

Measured at this wave's head against the owner's release build of `cd850e3` on port 7099, the
before column read with GET alone. The after column is a release build of this branch scanning
this workstation's three sources into a fresh scratch catalog on 7199 with `[mirror] offline =
true`, `hash_policy = "never"`, `[node] discovery = "none"` and no `[lookup]`; 244 artifacts
before, 246 after, so the two columns are the same disk a day apart rather than the same
catalog. `q=` totals, before then after: `wan 2.2` 10 then 6, and all six are Wan files where
four of the first ten were `Ministral-3-14B-Instruct-2512`, `NVIDIA-Nemotron-Nano-9B-v2-NVFP4`
and two `rStar-Coder` parquet sets; `want` 4 then 2, the two lost being cards holding `wanted`
or `wants`; `illustr` 3 then 1, where the plan expected 2: the Qwen card is gone as intended
and `oneObsession_v24` is not in the after column because the scratch catalog has no lookup and
its base reads `sdxl` rather than `Illustrious`; `gemma` 17 then 17; `pony` 1 then 1,
`obsession` 1 then 1, `gpt-oss` 1 then 1, `Q4_K_M` 6 then 6. The gpt-oss row reads
`shard_total` 15, `incomplete` true, the `files` fact `1 of 15 shards, incomplete` and the
triage reason `1 of 15 shards present, incomplete`; 15 and not the 14 the plan expected,
because the total comes from the index document, whose weight map names
`model-00000-of-00014.safetensors` through `model-00014-of-00014.safetensors`, fifteen files.
The three numbers, queue at `limit=500`, `/api/v1/stats` `triage` and the `state:triage` total,
were equal after every step: 24, 24, 24 before any verdict; 23, 23, 23 after confirming
`Kokoro-82M` as `emb`; 23, 23, 23 after a base-only override of `embedding_model.ckpt`, where
`cd850e3` would have read 22, 23, 23; 22, 22, 22 after rejecting `classifier.ckpt`; 23, 23, 23
after `DELETE /api/v1/triage/noise/1`, which answered `artifact_id: a_19` and left that row at
state `triage`, role `unk`, where `cd850e3` left it `local` and off the queue. The rail shows
31 base buckets and none of them holds artifacts that disagree on a label. A loadout of
`cyberrealisticPony_v160.safetensors` and `anima_context_detailer_base10.safetensors` exports
two `ln -sf` lines naming absolute paths under `/home/johnz86/GIT/ComfyUI/models/`.
`hoard doctor` on the scratch catalog exits 0 and prints `ok   catalog   format 19,
generation 4`. A screenshot of `http://127.0.0.1:7199/#/index?q=gpt-oss` shows `files` reading
`1 of 15 shards, incomplete` in the inspector's DERIVED FACTS.

Tests whose assertions changed, and why. `format_version_is_eighteen` became
`format_version_is_nineteen`. Every `record.triage_cleared` assertion in
`catalog/tests/rejection.rs` went with the field.
`a_verdict_that_leaves_the_role_unsettled_keeps_its_path_in_triage` moved from `(0, 1, 1)` to
`(1, 1, 1)`, because the queue no longer drops a record a verdict left unsettled.
`a_verdict_on_an_incomplete_set_leaves_it_queued_and_the_three_counts_equal` and
`an_incomplete_sets_reason_names_its_present_count` in `catalog/tests/reads.rs` and
`a_lone_shard_queues_with_a_reason_and_is_not_served_as_a_model` in `catalog/tests/scan.rs`
were asserting the reason that named no total and now name it.
`every_listed_base_extends_an_architecture_and_is_spelled_by_its_label` gained the assertion
that a listed base whose key hoard's rules produce, and which is not a lineage with a label of
its own, is spelled by that key.

What is left, in the order it will bite:

- The rail still runs one count query per shown bucket, 48 of them at the cap. `count_buckets`
  already tallies every base exactly once over the same set those queries intersect with, so
  every bucket's prefix-rule count is a sum over that tally; it is a performance wave with the
  1M harness number, not a correctness one, and at 246 artifacts nothing shows.
- A mirror pull never joins a set's record, so a set completed by pulls stays `incomplete`
  until a scan walks it. The watcher's scan is what completes a set today.
- A rules-inferred lineage and a Civitai-listed one agree only because `infer/base.rs` takes
  `Base::lineage_label` for the label; nothing pins that agreement for a lineage a future rule
  spells differently.
- The comfy export of a whole shard set links the anchor file `holds_whole` prefers, and an
  item whose held row records no `abs_path` reads `not on this node`; whether ComfyUI wants
  the set's directory is the owner's call.
- `Catalog::remove_noise` answers `Option<Option<ArtifactId>>`, the plan's own interface; an
  enum naming the three outcomes would read better at the one caller.

The review of this wave found two defects and six lesser points, fewer than a wave of this
size usually draws; both defects were introduced by the wave and closed before it landed:
`enter_triage` moved a `served` path to `triage`, which `leave_triage` then wrote back as
`local`, so a reject and un-reject cycle downgraded what the mirror served (pinned by
`un_rejecting_a_served_path_keeps_it_served`); and the `civitai` fact was appended after the
role rows while the contract said before, so `common::facts` now takes the listing and places
it (pinned by `the_civitai_fact_sits_between_the_files_and_the_role_rows` in
`api/facts_tests.rs`). A generated card is headed by a file only when a person opened one, a
later listing for the same bytes now updates the trigger words it wrote before, and the rail's
keep rule returns at once when the query names no base.

## Latest wave: a word inside a name, a lineage by its name, a lookup after hashing

Three things that made the search bar and the BASE column read wrong, and one that made the
Civitai worker ask before there was anything to ask about.

`crates/hoard-index/src/catalog/search/tokens.rs` is hoard's own tokenizer. It cuts at every
character that is neither a letter nor a digit, at every step between a letter and a digit,
and before an uppercase letter that follows a lowercase one unless another uppercase follows
it, so `cyberrealisticPony_v160` is `cyberrealistic`, `Pony`, `v`, `160`, `PONYTAIL` stays
one word and `LoRA` stays one word. `text_analyzer()` wraps it in the default analyzer's own
filters, `RemoveLongFilter(40)` then `LowerCaser`, and `register(index)` puts it on an index
under the name `hoard_words`, which `search/schema.rs` writes into the `text` field's
indexing options. Every index the catalog creates or opens registers it before it makes a
writer: `search/build.rs`, `search/refresh.rs` and the in-memory `one_segment` of
`search/mod.rs`, which is the whole list
`grep -rn "Index::create_in_dir\|Index::open_in_dir\|Index::create_in_ram"` returns. The
query side no longer reaches for `TokenizerManager::default()`: `QueryFields` carries a
`text: TextAnalyzer` taken from the open index with `index.tokenizer_for_field(fields.text)`,
at the three sites that build it (`search/answer.rs` twice, `search/inspect.rs` once), so
index and query cut identically or the query fails loudly. `trigger_text` keeps the `default`
analyzer, because `trigger:` is a whole-value term match. One fact the plan had missed went
with it: the grammar lowercased a free-text token's `values` before the analyzer saw them,
which destroyed exactly the case steps the tokenizer cuts on, so `search/grammar.rs`'s
`text_token` now keeps the case the user typed and leaves `normalized` lowercased as it was.

A lineage is stored under its own key. `hoard_core::Base::LINEAGES` is the one table naming
`pony`, `illustrious` and `noobai` beside the architecture each runs on and the label it
prints (`Pony`, `Illustrious`, `NoobAI`), and `Base::architecture`, `Base::lineage_label` and
`Base::lineages_under` are how everything else reads it. `Base::matches` gained the two
clauses that make a lineage select its architecture and be selected by it, and never a
sibling. `query.rs::base_filter` adds one term per lineage of the architecture asked for, so
`base:sdxl` still answers a Pony file and its rail bucket still counts what a click on it
answers; `base_compat` adds the same terms plus the prefixes of the lineage's architecture,
so a Pony host accepts a plain SDXL adapter. `infer/rules.rs` lost its own `LINEAGES` and its
three `BASE_ALIASES` rows now map a lineage to itself; `infer/base.rs` reads the core table
and falls back to the lineage's label when a metadata spelling named it; `infer/lookup.rs`
keys its lineage rows on the lineage and spells every label the way Civitai writes it
(`Illustrious`, `Flux.2 Klein 4B`, `ZImageTurbo`), and `refine` compares architectures as
well as keys.

The Civitai worker no longer runs on a timer alone. `Coordinator::finished()` counts scans the
lane ran to their end (`Lane.finished: AtomicU64`, incremented in `run_lane` after
`lane.run(item)`), and `hoard-mirror`'s `Cadence` reads it beside `Coordinator::busy()`: a
pass runs only while no scan is running or queued, again as soon as one has finished, and
otherwise every fifteen minutes; the worker looks every ten seconds. `spawn_lookup` takes the
coordinator and `hoard_server::start_lookup` hands it over. That is what makes the first pass
on a rebuilt catalog ask about the hashes the startup scan restored rather than about the few
that survived it.

`catalog::FORMAT_VERSION` is 18: the catalog is deleted and rebuilt from disk. The schema of
the `text` field changed and so did the stored spelling of every lineage, so a catalog stamped
17 holds terms cut the old way and bases spelled the old way.

Measured at `77ef442` against a baseline at `5af2c4d`, both a release build of the wave
worktree scanning this workstation's three sources into a fresh scratch catalog on port 7199
with `[mirror] offline = true`, `[index] hash_policy = "never"` and `watch = false`, no
`[lookup]` and `[node] discovery = "none"`. `artifacts` 240 and `triage` 23 in both columns.
`q=` totals, baseline then after: `pony` 0 then 1, `obsession` 0 then 1, `souls` 0 then 1,
`illustrious` 0 then 1, `cyberrealistic` 1 then 1, `gpt-oss` 1 then 1, `Q4_K_M` 6 then 6,
`loxsSoulsMix` 1 then 1, `Qwen3-VL-8B` 3 then 4. The one that moved where nothing was expected
to move is the last: the fourth row is `qwen3vl_8b_fp8_scaled.safetensors`, which the coarser
cut left as `qwen3vl` plus `8b` and no query spelling could reach. `base:sdxl` 5 then 5,
`base:sdxlpony` 1 then 0, `base:sdxlillustrious` 1 then 0, `base:pony` 0 then 1, `base:Pony`
0 then 1, `base:illustrious` 0 then 1. The BASE column of `cyberrealisticPony_v160` reads
`sdxl pony` then `Pony`, and of `loxsSoulsMixPerfectDoll_v30` `sdxl illustrious` then
`Illustrious`. The compat list of `cyberrealisticPony_v160` is the same 42 names in both
columns. `hoard doctor` prints `format 17` then `format 18`.

Tests whose assertions changed, and why. `format_version_is_seventeen` became
`format_version_is_eighteen`, because the format moved. Every assertion naming `sdxlpony`,
`sdxlillustrious` or the labels `sdxl pony` and `sdxl illustrious` - in `infer/base.rs`,
`infer/lookup.rs`, `sniff/mod.rs`, `catalog/tests/lookup.rs`, `catalog/tests/brands.rs` and
`hoard-mirror`'s `lookup.rs` - was asserting the suffix spelling this wave removes, and now
names the lineage and its label.
`every_listed_base_extends_an_architecture_and_is_spelled_by_its_label` now asks that a listed
base's architecture extends one hoard's rules name, because a lineage key no longer starts
with its architecture, and additionally pins that a lineage's listed label is the label
`Base::LINEAGES` gives it.

What is left, in the order it will bite:

- The rail's bucket cap was raised from 32 to 48 right after this wave landed, because on this
  workstation `base_total` is 49 and the `Pony` row was one of those cut. Each shown bucket
  costs one count query per search, so the cap is a cost the owner can lower again; the
  `perf-reviewer` has not measured 48 at a million artifacts.

## Latest wave: a confirm settles the paths it clears

A confirm wrote the role at full confidence and set `triage_cleared`, which took the artifact
out of the queue and out of the summary's `triage_artifacts`, but it never touched an entry,
so every path it had stayed in state `triage` until a rescan and the `state:triage` shelf read
one above the queue after every confirm. `catalog/verdict.rs` now has one verdict path,
`Catalog::record_verdict`, that both `put_override` and `clear_triage` call with a private
`Verdict` value: it loads `Counters` once, opens one transaction, touches the record, writes
what the verdict says on it, decides `needs_triage` once off the record it is about to store
and, when the record no longer needs triage, moves every `triage` path of the artifact to
`local` through the private `leave_triage` - the loop `clear_triage` already had, moved and
shared - with `DerivedShift` around each entry and `log_changes` and `counters.save` closing
the transaction. `put_override` kept only its `check_verdict` call and the `Override` value it
builds; neither verdict opens a transaction of its own any more. An incomplete set stays
queued through a confirm, so `leave_triage` never runs for it and its paths stay `triage`,
which is what `a_confirm_on_an_incomplete_set_moves_no_path` pins. A peer's row keeps its
state: `State::Triage` is written only for a path whose bytes are on this node, so the move
touches no other node's row, a `triage` flag such a row carried is cleared without its state
changing, and `Node.filed` is unmoved on both nodes because the move is on one node and
`DerivedShift` nets it to zero.

`catalog::FORMAT_VERSION` stays 17. No record changed shape: `ArtifactRecord`, `Entry`,
`Override` and `Noise` keep every field and the wave only changes which values a verdict
writes into fields that already exist. A catalog stamped 17 is therefore read identically
before and after, which also means a confirm made before this wave keeps its stale `triage`
entries until those files change or the catalog is rebuilt.

Measured on this workstation's real files at `d393c09`, a release build scanning the same
three sources into a fresh scratch catalog on port 7199 with `[mirror] offline = true`,
`[index] hash_policy = "never"` and `watch = false`, no `[lookup]` and
`[node] discovery = "none"`. The three numbers are the `/api/v1/triage?limit=500` length, the
`/api/v1/stats` `triage` field and the `/api/v1/search?q=state:triage` total, read after each
verdict. Before any verdict: 23, 23, 23. After confirming `a_12`,
`models/hexgrad/Kokoro-82M` `Kokoro-82M`, an ordinary queued artifact guessed `unk`, as a
lora: 22, 22, 22 - where before this wave the shelf stayed at 23 until a rescan. After
confirming `a_18`, `models/openai/gpt-oss-120b` `gpt-oss-120b`, the incomplete set, as an llm:
22, 22, 22, unmoved, because a verdict cannot make a set complete and the set stays queued
with its paths in `triage`. After rejecting `a_19`,
`models/speechbrain/spkrec-ecapa-voxceleb` `classifier.ckpt`, which answered one noise path:
21, 21, 21. `hoard doctor` on the same catalog afterwards exits 0 and reports
`ok   catalog   format 17, generation 1`. The gate moved from `is_queued` to `needs_triage`
after that run, on review; the readings stand, because a confirm naming a role, a confirm on
an incomplete set and a rejection take the same branch under either predicate, and the case
where they differ, a verdict that names no role, is pinned by
`a_verdict_that_leaves_the_role_unsettled_keeps_its_path_in_triage` rather than measured.

Left open. `DELETE /api/v1/triage/noise/{id}` still does not clear `rejected` on the record,
so un-noising a path does not put the artifact back in the queue until a scan re-stores it. It
is not fixable at this format version: a `Noise` row holds a path and nothing leads from a
path back to a record. `Repository` indexes only its identity key, not `root_path`, and
`Entry` indexes `artifact_id`, `content` and its location, never an absolute path, so tying
the row to its artifact today means a primary scan of every repository to find the root that
prefixes the path, which is unbounded at a million repositories. The fix is a field or an
index - `artifact_id` on `Noise`, or a secondary key on `Repository.root_path` - and so a
`FORMAT_VERSION` bump, to be taken with the next one. Separately, a verdict that leaves the
role unsettled - `PUT /api/v1/artifacts/{id}/override` with a base and no role, or a confirm
whose role is `unk`, both of which the routes accept - sets `triage_cleared` without
settling anything, so `needs_triage` stays true while `is_queued` goes false. The move is
gated on `needs_triage`, the predicate the scan (`scan/store.rs::entry_state`) and the summary
(`scan/counters.rs::SummaryPart::of`) write from, so the paths stay `triage`: the summary and
the shelf count the artifact while the queue does not, exactly as before this wave, and a
rescan of the same file cannot flip it back. `is_queued` differs from `needs_triage` only by
`triage_cleared` on a record that is not incomplete, which is why every other verdict sees the
two agree. The same summary-against-shelf split shows on an artifact whose only path was
retired: `SummaryPart::of` counts a record that needs triage whether or not any entry holds
it, so the summary total can stand above a queue and a shelf that agree with each other. F21
(the facet rail) is untouched.

## Latest wave: let a card describe only the files at or below its own directory

A revision kept one frontmatter, the shallowest `README.md`'s, and handed it to every artifact
of the revision, so the one README inside
`comfy/text_encoders/gemma-3-12b-it-qat-q4_0-unquantized` gave 18 unrelated encoders the
trigger words `gemma3, gemma, google` and made `q=gemma` find them.
`catalog/naming.rs::InferenceContext` now keeps one frontmatter per README that states any,
keyed by the directory the README sits in, and answers `frontmatter_for(rel_path)` from the
nearest one at or above that file's own directory, through the same `nearest` / `covers` pair
that already resolves `config_for` and `quant_config_for`; `nearest` became generic over the
document type so all three lookups share it. `catalog/scan/plan.rs::prepare_entry` is its one
caller. A README that states no frontmatter is not listed, so it shadows no card above it.
The stored card is still one per revision, the shallowest, but `models::RevisionText` now
carries the `directory` that card sits in and `RevisionText::text_for` is the one place the
scope of the indexed card is decided: `catalog/search/build.rs::Indexing::resolve` holds the
row rather than its text in its one-element cache and hands a document the card only when the
card covers that entry. Every writer of the record passes
`models::CardText { directory, body }`: the scan from `InferenceContext::card()`, a pull from
the pulled file's own directory, a seed from the revision root, and `move_revision_card`
carries the directory with the body. A card that moves to another folder keeping its words
answers `true` and has its revision's artifacts logged, because it now describes other files.
`catalog::FORMAT_VERSION` is 17: the catalog is deleted and rebuilt from disk.

Measured on this workstation's real files at `ab79f89`, a release build scanning the same
three sources into a fresh scratch catalog on port 7199 with `[mirror] offline = true` and no
`[lookup]`, against the same run at `cd24004`: exactly 1 row carries the trigger
`gemma3, gemma, google`,
`comfy/text_encoders/gemma-3-12b-it-qat-q4_0-unquantized/model.safetensors`, where 19 did
before; `q=gemma` answers 17 rows where it answered 32, and none of
`ministral-3-3b.safetensors`, `mistral_3_small_flux2_bf16.safetensors`,
`byt5_small_glyphxl_fp16.safetensors`, `clip_l.safetensors`,
`umt5_xxl_fp8_e4m3fn_scaled.safetensors` or any `qwen*` file of that folder is among them,
while the three `gemma*` files at the folder's root stay on the strength of their own names.
Exactly 18 rows changed trigger, all of them in `comfy/text_encoders`, each falling back to
its own file name; no hf-cache and no lmstudio row moved. Artifacts 239, triage 23 (the queue,
`/api/v1/stats` and the `state:triage` total all 23), `base:sdxl` 5, `base:sdxlpony` 1 and
`base:sdxlillustrious` 1 are unchanged; the catalog stamps format 17.

One test changed its assertion: `format_version_is_sixteen` became
`format_version_is_seventeen`. The new `catalog/tests/cards.rs` holds the scope tests, and
`pull_into` moved from `tests/mirror.rs` to `tests/mod.rs` unchanged so both files can use it.

Left open. The stored and indexed card is still the ONE shallowest `README.md` of a revision,
so a file under a deeper or sibling README that the shallowest does not cover reads that
README's frontmatter for inference but carries no card text in search. A README that changes
does not re-infer the unchanged weights it describes until those files change or the catalog
is rebuilt, because `scan/walk.rs::decide_unit` prepares only changed entries; that predates
this wave. Two `README.md` pulled at the same depth into one revision take the card in turn
(`pulls.rs::is_revision_card` refuses only a shallower one), so the last pulled wins and now
sets the card's directory too, where a scan keeps the first it saw at the minimum depth; that
also predates this wave. F21 (the facet rail) is untouched.

## Latest wave: say a partial set is partial on its row, pull the set, let a rejection end it

Four things an incomplete set got wrong, and one flag each. A row now carries `incomplete`
(`hoard_index::row::SearchRow`), filled from the record in
`catalog/search/answer.rs::assemble_row`, the one place a row is built: the field is read off
`ArtifactRecord` rather than out of a Tantivy document, so no search document and no
`FORMAT_VERSION` changed for it, and `hoard-server`'s `api/common.rs::RowView` re-exports it.
`SearchRow::is_set` is the question the pull line actually asks - more than one file present,
or fewer present than declared - and `PullArtifact::set` replaces the `shard_count` that
`api/common.rs::hf_scope` was misreading as "is this one file": `openai/gpt-oss-120b`'s line
is now the set's two `--include` globs instead of the one shard the disk holds, and
`companion_of` no longer offers a lone shard as anybody's companion. `api/loadouts.rs`
resolves "held" through one predicate, `holds_whole`, used by both `item_of` and `plan_line`,
so a pin on an incomplete set resolves `missing` or `elsewhere` and apply stops linking 1 of
15 shards; `PinResolution` gained no variant, because the drawer prints no per-item
resolution and what apply does is already right for `missing`. Finally `ArtifactRecord` gained
a stored `rejected` flag that `needs_triage` reads first, so the queue (`is_queued`), the
summary (`scan/counters.rs::SummaryPart::of`) and the entry state a scan writes
(`scan/store.rs`) all follow one predicate; `catalog/verdict.rs::clear_triage` sets it and, in
the same transaction, moves the artifact's `triage` entries to `local` through `DerivedShift`
and `log_changes`. `api/triage.rs::reject` records `walked_path` before `abs_path`, so a
rejected hf-cache file writes the `snapshots/` path the walker tests rather than the blob it
never visits. `catalog::FORMAT_VERSION` is 16: the catalog is deleted and rebuilt from disk.

Measured on this workstation's real files at `c5070a9`, a release build scanning the same
three sources into a scratch catalog on port 7199 with `[mirror] offline = true` and no
`[lookup]`: `repo:openai/gpt-oss-120b` answers one row with `incomplete` true, `shard_count`
1, state `triage`; its pull line is
`HF_ENDPOINT=http://127.0.0.1:7199 hf download openai/gpt-oss-120b --include 'model-*-of-00014.safetensors' --include 'model.safetensors.index.json'`;
the queue, `/api/v1/stats` `triage` and the `state:triage` total read 23, 23 and 23 before the
rejection and 22, 22 and 22 after it; the rejection answered one noise path, the
`snapshots/b5c939de.../model-00007-of-00014.safetensors` link and no `blobs/` path; the row
afterwards is `local` and still `incomplete`; the catalog stamps format 16.

Two tests changed their assertions. `the_queue_holds_every_state_triage_row_the_set_is_still_short`
lost its closing `clear_triage` step, which asserted that clearing cannot take an incomplete
set out of the queue: that is now exactly what a rejection does, and the confirm half of the
claim stays pinned by `a_verdict_on_an_incomplete_set_leaves_it_queued_and_the_three_counts_equal`.
`format_version_is_fifteen` became `format_version_is_sixteen`.

Left open. `DELETE /api/v1/triage/noise/{id}` does not clear `rejected` on a record no scan
has swept yet, so un-noising a path does not put the artifact back in the queue until it is
re-stored; the same was already true of `triage_cleared`. Still open after the verdict-states
wave; see above. Neither `ingest.rs` (the file the mirror fetched, around
:384) nor `pulls.rs` (the file a peer pull lands, around :400) ever clears `incomplete` on an
existing record: neither writer stores or re-reads the declared total, so a set completed
through the mirror or through a peer stays `incomplete: true` until a rescan re-derives it.
That staleness predates this wave; what is new is its consequence, because a loadout pin on
such a set now keeps resolving as not held and its pull line stays the set's globs until that
rescan runs. The inspector prints no `incomplete` fact. F19 (trigger words across a category
folder) and F21 (the facet rail) are untouched.

## Latest wave: ask Civitai what a file is, once, when the owner says so

A header proves an architecture and often no more: `oneObsession_v24.safetensors` reads `sdxl`
because nothing in its name, its folder or its tensors says `Illustrious`. With
`[lookup] civitai = true` a background worker now asks civitai.com by sha256 what each hashed
checkpoint, LoRA, ControlNet, VAE, embedding and upscaler on this node is - one request per
hash, one more every thirty days while the service does not know that hash, and at most one a
second - and refines the stored base with the answer. An answer that extends the architecture
hoard already proved refines it (`sdxl` to `sdxl illustrious`, which `base:sdxl` still finds),
one for an artifact whose header proved nothing names it at the lookup's own confidence of
0.7, one naming another architecture loses to what hoard has, and a user's verdict is never
overwritten: an artifact whose base a verdict settled is not a candidate at all, so it can
never take a place in a window it could do nothing with. Every answer leaves one `lookup`
evidence row saying what was listed and what it did, which is also what stops the same hash
being asked about twice. Answers live in `<data_dir>/civitai.redb` beside the digest memo, with
their own version stamp, so a catalog rebuilt after a format change restores every refinement
without a socket. `catalog::FORMAT_VERSION` is 15: the catalog is deleted and rebuilt from disk.

The switch is off by default and is ignored while `[mirror] offline = true`. Nothing but the
sha256 leaves the machine: no path, no file name, no `Authorization` header, no query string,
which `only_the_hash_leaves_the_machine` pins against wiremock. The lookup's client follows no
redirect, so a 3xx is a status that answers nothing rather than a way for the hash to reach a
host the configuration never named (`a_redirect_is_not_followed`), and it makes one attempt per
hash per pass, so a service that is rate limiting is asked once and the pass ends rather than
three times over. A stop reaches the worker inside the request it is waiting on, so shutting a
node down does not wait out a hung connection. To turn it on, add `[lookup]` / `civitai = true`
to `hoard.toml` and restart `serve`; the JOBS tab then shows a `lookup` job with subject
`civitai` for each pass of up to 256 hashes.

Expect compat lists to move the first time a pass lands, in both directions. Compat is a
two-way prefix test, so refining ONE side can end a match the two had before: a LoRA refined
from `sdxl` to `sdxl illustrious` no longer resolves against a host filed `sdxl pony`, though
both still answer `base:sdxl`. That is intended - a pony host and an illustrious LoRA are a
worse pairing than the bare architecture suggested - but it is a visible change to a list a
person may have learned.

The number that says the problem exists was measured by hand on 2026-09-17: of the 85 hashed
ComfyUI checkpoints and LoRAs on this disk, Civitai knows 72 (37 of 48 checkpoints, 35 of 37
LoRAs), and where hoard already has a base the listing refines the same architecture every
time and never contradicts it. No live run is part of this wave: turning the switch on sends
this machine's file hashes to a third party, and that is the owner's to do. The check that was
run is a release build with the switch absent and `[mirror] offline = true`, scanning the three
real sources into a scratch catalog on port 7199: the catalog stamps format 15, `/api/v1/jobs`
holds one `scan` job and no `lookup`, no `civitai.redb` was created, and artifacts 234,
triage 22, `base:sdxl` 5, `base:sdxlpony` 1 and `base:sdxlillustrious` 1 are the previous
wave's numbers unchanged.

One question for the performance reviewer. `Catalog::lookup_candidates` is a primary scan that
exits at the window, and it asks the memo through one read transaction for the whole listing,
but it still reads one cold `ArtifactDetail` row per hashed diffusion artifact it considers,
because the `lookup` evidence on that row is what says the artifact has been answered. In the
steady state, where every artifact has been answered, that is one LZ4 decode per diffusion
artifact every fifteen minutes, and at a million artifacts it is the only part of this wave
that does not obviously hold. The cheap fix is a flag on `ArtifactRecord` rather than a read
of the detail, which is a record shape change and so a wave of its own.

Two follow-ups this wave named and did not build. The inspector does not show Civitai's model
name and version, though the evidence detail carries both. Trigger words are not taken from
`trainedWords`, which is the other half of what a listing knows and what the TRIGGER column is
usually empty for.

## Latest wave: a partial set is partial, and a category folder is what discovery saw

A set was what held two files rather than what declared two, so `openai/gpt-oss-120b`, an index
naming fifteen files with one of them present, read as a whole local 4.1 GB `llm` and never
reached triage. (The names say fourteen and the `weight_map` names
`model-00000-of-00014.safetensors` through `model-00014-of-00014.safetensors`; the map is what
a declared total reads. What that download did land sits unfinished under
`models--openai--gpt-oss-120b/.cache/huggingface/download/`, fourteen `.incomplete` files dated
10 January, which the walker skips.) `shard.rs` now makes a set from a declared total of two or
more as soon as one member is present and unclaimed, so that row reads `triage`, `shard_count`
1 and the queue says `1 shard present, incomplete`; a name or a weight map declaring one file
declares no set. A declared set also claims its stem, so the files a map left over form no
phantom second set: `nvidia/NVIDIA-Nemotron-Nano-9B-v2-NVFP4` is one artifact named
`model.safetensors` where it read `model.safetensors (2 files)`. A shard's own file name yields
no trigger words, so eight rows lost triggers like `model of`, `data of` and `train of` and
carry none. Loose discovery stores the tool layout it recognized on the repository
(`Repository::tool_layout`, `comfyui` for a category folder of a ComfyUI root), and
`is_category_bin` reads that field instead of comparing a repository's org with its source's
name: `comfy/loras` still offers no whole-folder pull line, and a per-model folder sitting
directly under a plain loose source no longer loses one. Only the layouts whose top level
folders are categories stamp their folders: under a Kohya training root or a folder holding a
`.gguf`, a top level folder is one model and keeps its pull line, and only the files dropped
directly at the root are nobody's repository. A verdict cannot make a set complete, so an
incomplete record stays in the queue however it was cleared, which keeps the queue, the summary
and the `state:triage` shelf at one number.
`catalog::FORMAT_VERSION` is 14: the catalog is deleted and rebuilt from disk.

Measured on this workstation's real files, a release build scanning the same three sources
into a scratch catalog on port 7199, against the user's server on 7099: artifacts 232 to 234,
the two extra being files the 7099 catalog was scanned before they were on disk; the triage
total 22 to 22 (gpt-oss-120b joined, and `diffusiongemma-26B-A4B-it-NVFP4` left it through the
previous wave, which the 7099 process predates); and the triage total, the `triage queue` shelf
and the length of `/api/v1/triage` all 22. No repository on this disk is a per-model folder
directly under a plain loose source, so the narrowing of `is_category_bin` is proven by its
unit tests and by no regression on the fourteen `comfy/*` folders rather than by a row that
changed. `mistralai/Ministral-3-14B-Instruct-2512`'s `model.safetensors` keeps the trigger
`mistral-common`: it comes from the card's frontmatter, which outranks the file name, so the
shard-name guard does not reach it.

Two things this wave named and did not fix. A loadout pin on an incomplete set resolves to
`PinResolution::Present` and its pull line names the one shard that is there, because
`api/common.rs::hf_scope` reads `shard_count <= 1` as a single file: the copied command
re-fetches what the disk already holds instead of the set's `--include` globs. A correct fix
needs the record's `incomplete` flag carried on `SearchRow`, which is a record and index change
rather than a spelling one. Trigger words also bleed across a category folder: `naming.rs:72-79`
keeps one revision-wide card, the shallowest README, and `scan/plan.rs:496` hands
`plan.context.frontmatter()` to every artifact of the revision, so
`comfy/text_encoders/ministral-3-3b.safetensors` carries `gemma3, gemma, google` from a
neighbouring `gemma-3-12b-it-qat-q4_0-unquantized/README.md`. The rule to adopt is
nearest-directory scoping, exactly as `config_for` and `quant_config_for` already do; the
card-scope wave above adopted it.

## Latest wave: read the whole header before naming the base

Every base and role rule matched against a 64-name sample, and the sampler's own keyword list
covered 25 of the 92 fragments the rules name, so three 2,515-tensor SDXL checkpoints carried
no base although each holds `label_emb`. The sniffer now answers every rule's name test over
all of a header's tensor names into `TensorSummary::markers`, a vocabulary derived from the
rule tables themselves and never stored, and guarantees a sample slot to each tensor a rule
reads a shape from. Families are named by the exact keys ComfyUI detects them by, a LoRA by
the modules it patches, and a proven architecture is refined by a lineage from the file name
or the one folder the file sits in (`sdxlpony`, `sdxlillustrious`), which `base:sdxl` still
finds. A latent upscaler is no longer `any` and `mxfp4` is a precision. On this workstation's
files, checkpoints with a base went 23 of 55 to 48 of 55 and LoRAs 21 of 37 to 36 of 37,
`base:sdxl` 2 to 5, `prec:mxfp4` 0 to 1, and the compat list of `flux-2-klein-4b` 46 to 43:
the four LTX latent upscalers left it and the Flux autoencoder joined.
`catalog::FORMAT_VERSION` is 13: the catalog is deleted and rebuilt from disk.

Three things the wider read forced. `SWINIR_KEYS` lost `patch_embed.norm`, which every Swin
and ViT backbone carries and which read `birefnet.safetensors` as an upscaler that then
applied to every host; the remaining terms keep every SwinIR file here, and `role:upsc` still
counts 45. A lineage is read from the file name and the immediate parent folder only, never
from the rest of an absolute path, so a home directory named `pony` relabels nothing. `base:`
now filters one way, from the architecture down: `base:sdxl` answers 5 and `base:sdxlpony`
answers 1 rather than every plain `sdxl` file as well; `compat:` keeps the two-way test,
which is where that notion belongs.

One row still differs from what the previous review left: `diffusiongemma-26B-A4B-it-NVFP4`
reads `llm` from its `model.layers.`/`embed_tokens` pair rather than staying in triage, and
that is accepted. `ltx-2-19b-distilled-lora-384.safetensors` reads `qwenimage`, which is
correct: the file is byte-identical to `Qwen-Image-Lightning-4steps-V1.0.safetensors`. The
rail's brand row for `sdxl` was not added: `search/brands.rs` holds a pinned test forbidding
an sdxl anchor, which is a decision to take rather than a test to edit.

## Latest wave: the page describes the artifact that was opened

A repository page described the largest weights file of `main` whatever the index row opened
it: `Z-Detail-Slider.safetensors` (21.3 MB) showed the facts of a 7.6 GB LoRA beside it and
copied `hf download comfy/loras`, which is 38 files and 30.9 GB. The four repository routes
now take `artifact=<id>`, and the header, `facts`, `meta`, `paths`, `pull` and `compat`
describe that row, on the revision holding it unless a `revision` is asked for; FILES marks
the files it is made of. `PullCommands` gained `repository`, the whole-repository line, absent
for a category folder of a tool layout, whose generated card also stops offering one. One
writer, `api/common.rs::hf_arguments`, spells everything after `hf download`, so an LLM and its
single projector come in one line, a `gguf` set asks for no `.index.json` that was never
written, and the loadout `hf-script` export carries shard globs, `--revision` and
`--repo-type`. `L` creates a loadout named `loadout` on first use instead of toasting that
there is none. `hoard-server`, `web/` and one public function in `hoard_index::shard`;
nothing stored changed and `catalog::FORMAT_VERSION` stays 12.

## Latest wave: the any bucket is active only when the query names any

The base rail decided a bucket's `active` flag with `Base::matches`, whose `any` wildcard is a
compatibility rule, so `any` lit up beside every `base:` query and `base:any` lit every row of
the rail: on the live catalog `q=base:flux` marked 2 base buckets and `q=base:any` marked 17
over 45 rows, where each should mark 1. `selects` in
`crates/hoard-index/src/search/facets.rs` now compares `is_any` on both sides first and keeps
the prefix rule for every other pair, so each marks 1 and a token holding `any` beside a base
still marks both. `Base::matches` is unchanged, nothing stored changed and
`catalog::FORMAT_VERSION` stays 12.

## Latest wave: a name finds its file, and a pull names the revision holding it

Free text now runs through the analyzer the `text` field is indexed with and matches its words
as a phrase whose last word is a prefix, so `gpt-oss`, `Q4_K_M` and `loxsSoulsMix` answer 1, 6
and 1 against this workstation's caches where each answered 0; a `~` token matches every word
as a prefix in any order. The `hf` pull line of an artifact held outside `main` carries
`--revision <sha>`, so artifact `a_14`'s copied command downloads its 5 files instead of none.
Nothing stored changed shape and `catalog::FORMAT_VERSION` stays 12.

## Latest wave: a format bump no longer costs a rehash

A hashing pass now recalls a file's sha256 and git blob sha1 from `<data_dir>/digests.redb`,
a sidecar keyed by the inode, mtime and size that earned them, and reads a file's bytes only
when that stamp actually moved. The sidecar sits beside `native/` rather than inside it, so a
`FORMAT_VERSION` bump - which deletes `<data_dir>/native` and scans it again, throwing away
every stored `sha256` - now costs a walk and a stat per file rather than a re-read of every
byte the cache holds. `docs/native-catalog.md` under "Layout" has the file, its own version
stamp and the measured before and after.

## Latest work: the memory goal is closed

The goal was that a rescan holds one repository at a time rather than the whole pass. It
landed as `b10c30d` (a standalone vehicle to measure a rescan's absolute peak, not a
high-water-mark difference), `9dd3eb8` (discovery reads and stores one repository window at a
time instead of a source whole) and `5e06f56` (a rescan loads and retracts one repository's
stamps, revision sizes and seen set at a time), followed by two module splits that touched no
production code: the `search.rs` split at `4c32a46` and the `catalog/tests.rs` split at
`568ec5f`.

At 1M files, one repository per folder, the rescan peak is 272 MiB (it was about 870 before
`9dd3eb8` and `5e06f56`). A single repository holding every file is still read and held whole
by design, and its rescan peak is 847 MiB.

Every open item in `docs/finish-the-memory-work.md` is closed; `568ec5f`'s commit message
says so directly.

**Next steps**, in this order:

1. Two defects the run against this workstation's real caches found, recorded with their
   evidence in `docs/native-catalog.md` under "A scan of this workstation's real caches":
   `hoard scan` on an empty data directory builds the catalog through `Catalog::open_or_scan`
   and then rescans it again in `src/cli/scan.rs::run`, reporting the second pass as
   "unchanged"; and a source's byte total sums every filed path's size, so hf-cache snapshot
   revisions that share a blob count it once per revision, 23.5 per cent over what the source
   holds on disk here. Whether a source's bytes should mean disk held is the owner's call before
   it is a fix.
2. Two correctness gaps neither closed goal touched: `native_model::Model::native_model_decode`
   panics on a stored row shorter than its 8-byte header instead of returning an error, and a
   retirement killed between `retire_entries` and `recount_retired_paths` leaves `path_count`
   stale in a way `hoard doctor` cannot see, because both the stored summary and a fresh count
   read the same stale value. Two more found while fixing the format rebuild: a run killed
   after `Catalog::create` makes the catalog directory but before `catalog.redb` exists leaves a
   directory that `open_or_scan` cannot recreate, so the next start fails on `create_dir`; and
   `hoard_mirror`'s `batch::tests::what_is_left_is_written_by_a_flush_and_by_a_shutdown` hung
   once while a release build and a rescan shared the CPU, and passed on a quiet rerun.
3. Splitting the files still over 1,500 lines: `crates/hoard-mirror/src/resolve.rs` (2,643),
   `crates/hoard-server/src/api/tests.rs` (2,426), `crates/hoard-net/src/peer.rs` (2,413) and
   `crates/hoard-index/tests/scale.rs` (1,837).
4. Only if the real run says memory still matters, sizing the redb read cache, `CACHE_BYTES` in
   `crates/hoard-index/src/catalog/mod.rs`, which is up to 230 MiB of the 272 MiB peak.

## Memory delivery, 2026-09-08 (commit `51418ad`)

Delivered continuous hashing and metered index commits, recovered selectively from
`fef969a` against immutable baseline `bdf2964`. Mixed-size hashing reaches its cancellation
checkpoint in 9.4 ms versus 570–574 ms; large-card rebuild memory is 350–382 MiB versus
744–772 MiB. Metering adds some rebuild overhead on small records. All final 100k/1M
measurements and the eight-request direct/HTTP gates pass; the selected HTTP maximum is
102.93 ms. See [the report](native-catalog.md#memory-verification-2026-09-08) for boundaries,
tradeoffs, exact source hashes, commands, and the single compressed evidence archive.

Final checks: 888 tests passed, workspace build, warning-free Clippy, formatting, actual
CLI/server smoke tests, cancellation/resume/hash equivalence, collapse/deletion refresh,
and interrupted publication/replay. The ignored measurement fixtures actually ran.

Format stays 7. No usage schema, event overload behavior, or source discovery change is
retained. Summary folding lacks isolated evidence and was removed. Usage rekeying and
writer/coalescing changes are deferred: format-8 recovery deletes and rescans rather than
preserving the existing catalog. Source-wide retirement bookkeeping, pending usage batches,
and oversized artifact groups remain unresolved; the full memory redesign is not complete.

All task-created worktrees, binaries, synthetic catalogs, and the external
`/home/johnz86/.cache/hoard-memory-evidence` directory have been removed. No experimental
branch remains and nothing was pushed. No user's catalog was used. Lost measurements from
the first interruption are excluded. Do not restart the restore-all-waves process; follow
[the remaining acceptance requirements](finish-the-memory-work.md).

## The repository

- The main checkout is `/home/johnz86/GIT/hoard`, on branch `master`; no experiment worktrees
  remain. **Nothing has been pushed.** The current memory task
  requires preserving populated format-7 catalogs; older compatibility assumptions do not
  waive that requirement.
- `docs/superpowers/specs/2026-09-06-remaining-work-goal.md` is the brief every wave now
  follows: what "remaining work" means, the finish condition that ends the run, and the rule
  that performance is measured once, in the last wave, rather than in each one.
  The run is finished: `docs/superpowers/history/2026-09-remaining-work-review.md` is the
  review of what each wave changed, the final numbers, what the closing reviews found and
  the recommendations for what comes next.
- `docs/finish-the-memory-work.md` records the delivered scope and remaining acceptance
  requirements as they stood when that document was written; the one-repository-at-a-time
  rescan goal it left open has since landed and closed, as "Latest work" above says.
- **The predictable-memory run of 2026-09-07 was reverted.** It set five waves and landed
  four, measured nothing, and was halted by the owner. Only the two components documented
  above have since been recovered. The original commits remain reachable at the tag
  `reverted/2026-09-07-predictable-memory`.
  `docs/superpowers/history/2026-09-07-predictable-memory-reverted.md` says why, what the
  process got wrong, and - the part worth keeping - the facts its reviews established about
  tantivy's per-document allocation, native_db's scan semantics, what the gate tree cannot
  show, and the hazards any future batching of a source's repositories has to respect. Read
  it before planning optimisation work on this catalog.
- The native_db (redb) plus Tantivy catalog replaced the SQLite index and is merged. Every
  command, every route, both faces and gossip read and write it, and no SQLite code remains.
  `docs/native-catalog.md` is the current-state document;
  `docs/superpowers/history/2026-09-native-catalog-waves.md` holds the condensed record of
  how it was built.
- Every catalog record has one shape at `native_model` version 1, and there are no
  migrations. `docs/superpowers/specs/2026-09-05-native-catalog-decision.md` and
  `docs/superpowers/research/2026-09-05-native-db-tantivy.md` are the decision and the
  dependency research; they stay. The wave plan files under `docs/superpowers/plans/` are
  deleted - git keeps them, and a stale plan in the tree is a trap for the next agent.
- A run against the real configured sources, as opposed to a synthetic fixture in a test, uses
  `cargo run --release -- serve`. A debug build hashes at a fraction of a release build's
  throughput, so a first scan of a large cache appears to hang; the
  `[profile.dev.package.{sha2,sha1,xxhash-rust}]` stanzas in the root `Cargo.toml` close most
  of that gap by giving the three hashing crates full codegen in a dev profile, but not all of
  it, because the code around them is still unoptimized.
- Phases 0 to 11 are done. `docs/feature-plan.md` holds the table. The TREE and DISK views
  are not an unfinished phase: they wait on a design decision and are recorded under that
  document's "Risks and open decisions".
- Current memory-delivery checks are recorded above. Historical checks and measurements
  below describe their original commits, not a fresh verification of this delivery.
- Waves 1 to 9 of the goal are on `master` and the run is measured. The measurement is in
  `docs/native-catalog.md` under "Measured": the whole scale harness at 100k and 1M against
  a `3a9fa1f` column rebuilt and run in the same session over the same tree, the eleven
  routes at 100k in both columns, the supplemental fixture in both, and a fresh scan of this
  workstation's three sources with `doctor` and `doctor --repair` after it. Every standing
  gate at 1M is met. Wave 8's three route misses are settled under "The fix wave's rows":
  two of them were the timing protocol rather than the code, and all three read faster at the
  head than at the `3a9fa1f` column beside them.

## Running the loop

Work runs as waves. Each wave is one worktree, one branch, one plan file, one fresh
implementer agent, a verification pass, a review, and **one squashed commit on `master`**.
The implementer's per-task commits are checkpoints on the branch and never reach `master`;
the plan file is never committed. The skills carry the contracts so no prompt has to repeat
them.

1. **Open the wave.** From the main checkout:
   `git worktree add ../hoard-<name> -b wave/<name>`. Then write the plan following
   `.claude/skills/hoard-write-plan/SKILL.md` to
   `../hoard-<name>/docs/superpowers/plans/<name>.md`, untracked. Anchor its Verified facts to
   the commit the branch starts from and grep every symbol it mentions before you hand it
   over. The plan carries the subject and body of the wave commit `master` will receive. Run
   every gate grep the plan names and quote the count each one found; for every invariant a
   task relies on, name the file and function that enforces it. Name the pinned tests: the
   tests of the waves this one builds on that must keep their assertions.
2. **Dispatch the implementer.** The `wave-implementer` agent, given the worktree path and
   the plan path, and nothing else. **Do not downgrade the model.** The reverted run of
   2026-09-07 ran every wave on `sonnet` to save tokens and spent far more of them on review
   rounds than it saved; the agent definitions carry the model and the dispatcher does not
   override it. It follows `.claude/skills/hoard-implement-plan/SKILL.md`:
   preflight in the worktree, the AGENTS.md rules, the checks, foreground-only process
   hygiene, a record of every deviation from the plan, one checkpoint commit per task, and a
   report. **A review finding is not automatically a task.** A small correction can be
   appended to the wave; anything larger means the plan was wrong, and a wrong plan is
   rewritten rather than patched by appending tasks to the agent that followed it. Appending
   round after round is what makes one transcript write something and then delete it, which
   is what the owner saw and stopped in the reverted run.

   The report is written so you can verify without reading the transcript, and section 7 of
   that skill fixes what it contains: the commit hashes and subjects in order, every
   deviation with what the plan said and what the code said, the final output line of each
   check quoted rather than "all checks pass", the measurement tables with a commit per
   column, the tests whose assertions changed and why, what was left for a human, and a
   clean `git status --short` with the `pgrep` result. A report missing a section is a
   report to send back.

   An implementer that stops early - a deviation that makes a task's outcome wrong, a check
   it cannot get green, a decision that is not its to take - stops after a checkpoint that
   builds and says so in its first line. Read its reasoning, take the decision yourself, and
   resume it with a message naming the next task; a fresh agent would re-derive the whole
   plan. An implementer that appears to be waiting on a long command is waiting on a
   foreground command, which is what the skill requires: let it finish, or raise the
   timeout and resume it. Never ask it to poll, and never start a background process on its
   behalf.
3. **Verify and review, in the worktree.** `/hoard-verify` there: `cargo fmt --check`, clippy
   with `-D warnings`, `cargo test --workspace`, `cargo build --workspace`, plus
   `cargo test --release -p hoard-server --lib assets::` after any change under
   `crates/hoard-server/` or `web/`, and `cargo doc --workspace --no-deps` once. Then
   `perf-reviewer` for a performance wave, following
   `.claude/skills/hoard-perf-review/SKILL.md`; `rust-reviewer` otherwise. Both are
   read-only and review `master..HEAD` in the worktree. The reviewer's brief names the waves
   this change set builds on, not only `master`. Do not run a build while a reviewer is
   measuring, or dispatch a measuring reviewer beside anything else that uses the disk.
   Fixes from the review go on the same branch, as more checkpoints, before landing.
3b. **Halt and ask, do not push through.** Stop the run and put it to the owner, with the
   numbers, when any of these is true. Each one was true in the reverted run of 2026-09-07
   and none of them stopped it:

   - a review finds more than it found on the previous wave, rather than fewer;
   - a wave introduces a defect that did not exist before it, especially one that can lose
     data;
   - a wave lands with nothing measured, on a run whose purpose is a measurable property;
   - the goal document needs correcting for a second time.

   **Report the trend, not the events.** "Review found things and we fixed them" reads as a
   healthy loop and hides a rising defect rate. Say how many findings this wave drew against
   the last, and say plainly when a wave introduced a defect rather than only that it was
   closed.

4. **Land the wave.** From the main checkout on `master`:
   `git merge --squash wave/<name>`, then one commit whose subject and body are the plan's
   wave commit, ending with the two attribution trailer lines this session was given. Then
   `git worktree remove --force ../hoard-<name> && git branch -D wave/<name>`; the force is
   for the untracked plan file, which goes with the worktree. Update this handoff inside the
   wave (a task of the plan) so it lands in the same commit, never as a commit of its own.

For anything touching `crates/hoard-index/src/catalog/`, read
`.claude/skills/hoard-catalog/SKILL.md` first: the module map, the write path, and the
native_db and Tantivy semantics that the calling code does not show. For anything measured,
`.claude/skills/hoard-measure/SKILL.md`, including its like-for-like rule - day-to-day
timings on this workstation drift by more than most of the effects being measured, so a
"before" number is rebuilt and measured in the same session, never quoted from a document.

`/hoard-next-phase` turns the next unchecked phase of `docs/feature-plan.md` into a wave
plan and hands it to the same loop.

## Follow-ups

Every measured number and the narrative that explains it lives in `docs/native-catalog.md`
under "Measured": the disk gate and where the bytes are, a page anywhere in the order, the
tallying pass, the collapse path, the routes at 100k, the fix wave's rows and the thresholds
table that answers each gate once. What is left here is what is deliberately not code, and
what wave 9 looked at and left.

- **Page packing is answered and not worth doing.**
  `docs/superpowers/research/2026-09-page-packing.md` reads redb 2.6.3: the 42 to 55 per cent
  leaf fill comes from one split rule that always cuts at the byte midpoint and has no append
  case, so insertion order, a bulk load in key order and a dense rewrite all land at the same
  fill, `Database::compact` moves whole pages and never rebuilds a leaf, and
  `Builder::set_page_size` is compiled only under `cfg(any(fuzzing, test))`. The reachable
  saving is 0 to 5 per cent; the lever that works is payload, at about two bytes of file per
  byte off a row. Whether to act on it is the owner's call, and the recommendation is not to.
- **The five 1M query rows that were 1.5 to 3.6 per cent slower are back, unexplained.** The
  first page, the two midpoints, `role:lora` and the filtered query read within 1 per cent of
  the `3a9fa1f` column at the head, inside the spread of two runs of the same binary. The
  hypothesis for the gap was codegen around the collectors' inner loop - wave 4 gave every
  primary collector a per-document `Collapsing::offer` that did not exist before - and it was
  never tested: monomorphising the four collectors over a `const COLLAPSING: bool` would
  duplicate every call site and the rows no longer ask for it. If they slow again, that is
  the first thing to try.
- **A revision slot is emptied but never reclaimed.** A revision no ref reaches and no entry
  names is written back as a tombstone and its card is dropped, so an advancing branch costs
  one empty slot per old head rather than a card and a commit per head. Reclaiming the slot
  itself needs the entries of a retired revision removed rather than marked absent, which is
  the same decision as pruning a deleted path's row; neither is taken.
- **A path outside UTF-8 is identified but not stored.** The identity a scan computes is
  byte-exact, but `Repository.root_path` and `Entry.placed` hold `to_string_lossy` text, the
  walker drops a relative path that is not UTF-8, and `SourceConfig` cannot serialise a root
  that is not. Storing the bytes is the remaining half, and it is a record shape change.
- **A read-only open** is waiting on redb offering one, so searches could run beside a
  rebuild rather than beside a lock.
- **A retirement killed before its recount leaves path counts stale with nothing to find
  them.** `retire_entries` marks entries absent in one transaction and `recount_retired_paths`
  corrects the affected artifacts' `path_count` from `Counters.touched` afterwards, which is
  an in-memory set. A kill between the two loses it, and because both the stored summary and
  a fresh count read whatever `path_count` is stored, the two agree and `hoard doctor` sees
  nothing. Found during wave 4 of the predictable-memory run and deliberately not fixed
  there: it is a different pass, and fixing it properly means giving retirement a durable
  record of what it still owes, which is a design rather than an edit.
- **Discovery now reads a bounded window of repositories, and a single repository is still
  read whole.** `9dd3eb8` made a scan or rescan plan a source's repositories and read, store
  and walk them a window at a time rather than discovering the whole source into memory
  first, which is what brought the per-folder rescan peak down from about 870 MiB to 272 MiB
  at 1M files. A repository of a million files in one folder is still held whole by design,
  and that tree's rescan peak stayed at 847 MiB.
- The narrower items each carry their own paragraph in `docs/native-catalog.md` under
  "Remaining work": the tally's slot per term of a column, the sampled path's fallback
  recollecting, `sweep_artifact` decompressing a cold row to delete it, and
  `native_model::Model::native_model_decode` unwrapping a payload shorter than its header.

Decisions taken on 2026-09-06, by the project owner:

- **`hoard doctor` is diagnostic by default.** It reports drift and exits non-zero without
  correcting it; `hoard doctor --repair` recounts the derived totals, reports before and
  after and verifies the result. Counting is separated from storing: `Catalog::count_summary`
  reads one snapshot and writes nothing, `Catalog::repair_summary` is exclusive by signature.
  Doctor opens through `node::inspect`, which creates no directory, scans nothing, refreshes
  nothing and records nothing; a missing or foreign catalog is a finding it reports, not
  something it fixes on the way in. Read-only means no catalog row is written: native_db
  opens every table in a committed write transaction as it opens the file, so the file's
  transaction id advances whatever a caller does. **Done in this wave.**
- **Automatic rescans never compact.** `hoard catalog compact --directory <dir>` compacts on
  demand, takes the catalog to itself through the redb lock and reports allocated bytes
  before and after. A fresh scan still compacts at the end of building one, which is bulk
  construction. There is no scheduled server compaction until sustained churn is measured,
  and intra-page slack is not a promise of what a compaction returns: dense packing is a
  separate investigation. **The command is done in this wave; the rest is standing policy.**
- **Live rescanning ships, on by default for configured filesystem sources with an opt-out.**
  Its preconditions: manual and watched scans share coordination so neither runs over a
  source the other holds, the configured hash policy is preserved, an overflowed watch marks
  its source dirty rather than losing the change, Hoard's own writes under the data directory
  are excluded, shutdown joins the watcher, and the rebuild-versus-write boundary is
  enforced. Every one of them is met. `hoard_index::coordinate::Coordinator` is the one lane
  every scan under `serve` runs on, `POST /api/v1/scan` submits to it and `serve` drains it,
  and `Catalog::search_lane` serialises `rebuild_search` against `refresh_search` while a
  rebuild prunes only what it indexed. `crates/hoard-index/src/watch.rs` is now a producer
  for that lane: `hoard_server::start_watcher` starts it from `[index] watch`, each debounced
  burst is one `ScanRequest` with `Origin::Watched` under the configured hash policy, an
  overflow, a lost signal or a watch the platform refused reconciles every source with
  `Origin::Reconcile`, a timer does the same every `reconcile_interval_secs` and once at
  startup, the data directory, the index directory and the mirror cache root are excluded
  whole and resolved to absolute paths before the debounce so a root spelled relative to the
  working directory cannot feed a scan its own writes, `/healthz` carries `watch` and
  degrades only on a watch the platform refused, and `hoard_server::serve` joins the watcher
  before draining the lane. A configured root that is not a directory is reported in
  `WatchStatus.missing` and is not degraded, because a scan skips it with a warning too.
  `Watcher::start` walks every directory of every source synchronously to register inotify
  watches, and each directory costs one watch against `fs.inotify.max_user_watches`.
  **Done in wave 3.**
- **Managed sources stay visible.** `GET /api/v1/sources` lists every source the catalog
  holds, including a peer's namespace, and each row carries `can_scan`, true exactly when the
  configuration names it, and `managed_by`, one of `"downloads"`, `"pulls"`, `"peer"` or
  `"nobody"` for a stored source the configuration no longer names. The UI offers SCAN on
  `can_scan` alone and labels the rest; no source name appears in JavaScript, and the backend
  still answers 404 for a scan of a source it does not walk.
  **Done in this wave.**
- **A source dropped from the configuration is deleted by the next full rescan.** Taken by
  default in wave 5a, as the question wave 1 raised. `Catalog::rescan` now takes a
  `RescanScope`: `Configured` says the config it was handed is the whole configuration, so a
  stored source row outside it has been dropped; `Narrowed` says the caller chose a subset
  and retires nothing. A dropped source's entries, repositories and the artifacts that lose
  their last path are deleted, moving the derived counts through `DerivedShift` and logging
  each artifact through `log_changes`. They are deleted rather than left as `Remote` rows
  because a path of a dropped source is not a path this node stopped holding, it is a path
  this catalog stopped being about: a repository row whose `Source` is gone is rendered by
  `repositories()` under a default kind, is still counted by `Summary.repositories`, and
  files every path a second time if the source is ever re-added. The mirror, pull and peer
  source rows are never a scan's to retire, and a source whose root is merely unmounted
  stays, because the configuration still names it. The effect on "Managed sources stay
  visible" is that `managed_by: "nobody"` is now the state of a dropped source between the
  drop and the next full rescan rather than a permanent one. **Taken in wave 5a.**
- **The triage queue stays local and actionable.** A peer-only artifact stays searchable as a
  remote artifact and does not enter this node's queue: local evidence decides local triage.
  **Already the behaviour; recorded, no code.**
- **Phase 1 stays complete and Phase 11 is done.** Phase 1's SQLite implementation is
  history, not an unfinished phase. Phase 10 is done as of wave 3, now that the watcher
  starts, reconciles and shuts down through `serve`; TREE and DISK need a design decision
  before either can be implemented and are a risk rather than a phase. Phase 11's storage
  replacement is complete; the optimisation that continues is a list of follow-ups with
  their own acceptance criteria under `### Phase 11`. **Done in this wave.**
- **How should the base facet be counted?** Taken by default in wave 4: the rail is
  truncated to the 32 most populous bases by tally before the per-bucket count runs, so the
  searches a query pays for the rail are bounded by a constant rather than by how many bases
  the catalog holds. The prefix rule is unchanged, so a bucket's count is still what a click
  on it answers; a base outside the 32 is not shown and is not marked active, and a query
  naming it still answers. `docs/api/hoard-api.md` says so. Answering the prefix rule from
  ordinals, which would change what a base bucket means, remains the other way out and is
  still not an implementer's call. **Taken in wave 4.**

Smaller things, each verified:

- A shutdown costs a megabyte, not a pass. The scan lane raises one stop when it closes: a
  walk checks it between units, a hashing pass before each batch and once per megabyte of
  every file it reads, and requests that had not started are cancelled. Both passes leave the
  pass mark set and retire nothing, so the next start settles the counts and finishes the
  hashing. On this workstation, SIGTERM during the startup reconciliation's hashing is
  answered in 0.040 s where it took about 4.5 minutes.
- A file listing reads the artifact behind each file only for the screen that renders the
  `deduped` mark. The Hub metadata routes, the projection, the peer digest and the cache
  sweep asked for a mark they threw away, at one random point read per file.
- The triage threshold a catalog was scanned with is stored in its manifest and every later
  writer counts against it, so `hoard doctor` no longer fails a healthy catalog scanned with
  a non-default threshold.

- The hourly cache eviction sweep walks the mirror in chunks. `hoard-mirror`'s
  `cache/reconcile.rs` calls `Catalog::served_cache_entries_in`, which reads the mirror's
  repositories through one prefix scan of the identity key and hands each repository's
  served rows over in chunks of 1,024, measuring each chunk against the cache tree as it
  arrives; the rows that are gone are retired once at the end. The residual is that one
  repository's served rows are materialised at a time, which is what a location prefix scan
  costs.
- An entry re-served between the sweep's walk and its retire is retired anyway, and the next
  serve of it files it again. Pre-existing: the walk and the retire were two catalog calls
  before this wave and still are, and chunking the walk did not widen the gap.
- A rescan recognises a stored row by where it is filed - repository, revision ordinal,
  relative path - rather than by the absolute path its bytes were last found at, so an
  hf-cache file whose content changes in place is replaced instead of colliding on the unique
  location key. A revision found at a fresh ordinal still refiles its unchanged rows onto it,
  from a second index by repository and path that leaves out a path more than one stored
  revision holds. `docs/native-catalog.md` under "The write path" says so.
- An hf-cache revision carries a card and a config. A snapshot links `README.md` and
  `config.json` at blobs named by their hashes, and the sniffer classified the blob's name,
  so every document of a cached revision was `Unknown` and its inference context was empty;
  no hf-cache revision had ever had a card. `sniff::sniff_named(path, name)` reads the header
  from the blob and classifies by the entry's own relative path, and `sniff_path` is that
  function with the path's own name. That wave bumped `FORMAT_VERSION`, because a blob's
  inode, mtime and size do not change when the rule that reads it does and a rescan would
  settle every stored revision on its stamps; the bump rebuilds every existing catalog from
  disk, which is what puts the cards into one. It is 7 now, moved once more by wave 9. Wave
  8's workstation scan therefore reads and indexes the READMEs and configs of every cached
  revision: its scan time, its card count and its search answers all move, and nothing in the
  synthetic tree does, because the tree has no hf-cache source.
- `sniff::SNIFF_VERSION` and `Artifact::sniff_version` are gone. The constant was written
  into every artifact record by two sites and read by nothing, and its doc comment promised a
  re-read of every header that no code performed. A per-artifact stamp could not have
  delivered it either: the documents whose classification changed are entry records with no
  artifact of their own. A sniffing or inference rule that changes is a `FORMAT_VERSION` bump
  and nothing else, which `docs/native-catalog.md` now says under "What the scan produces".
- A query that lands between a rescan and the refresh that follows it answers with `stale`
  set, skipping the rows that rescan replaced, instead of failing on an entry the published
  index still names. A skipped row counts as a position walked, so `next_cursor` is offered
  as it would have been and paging does not stop short. `docs/api/hoard-api.md` under
  "Search" says what a caller sees.

`docs/native-catalog.md` under "Remaining work" holds the rest: a truncated record panicking
inside `native_model` before any hoard codec runs, `sweep_artifact` decompressing a cold row
only to hand it back to `remove` because native_db 0.8.2 has no remove-by-key, storing a path
whose bytes are outside UTF-8 now that its identity is byte-exact, reclaiming the slot of a
revision the walk no longer finds, and a read-only redb open once one exists. What a kill at
each stage leaves and how the next run completes it is recorded under "Recovery" in the same
document, with the two writers whose totals a kill still leaves to `hoard doctor`.
`docs/feature-plan.md`
under `### Phase 11` names which of them carry acceptance criteria as follow-ups of that
phase.
