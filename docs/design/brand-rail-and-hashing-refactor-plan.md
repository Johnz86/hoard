# Brand rail, projector pairing and the hashing pass: the correction plan

Historical decision brief. The implementation and subsequent refinements are recorded in
[current design status](README.md); the observations below refer to the cited commit.

Status: decided. This document corrects `docs/design/catalog-navigation-review.md` (proposal A)
and `docs/design/catalog-improvements-plan.md` (Wave 4, and the rail filter in Wave 3.4). Where
those two disagree with this one, this one wins. It also settles two things neither of them
covered: the `mmproj` companion files, and what the hashing pass does to a machine on first run.

It is not a wave plan. It is the brief a wave plan is written from, in the shape
`catalog-improvements-plan.md` had. `.claude/skills/hoard-write-plan/SKILL.md` says what the
plans themselves must carry.

No application code was changed to produce it. Every claim about current behaviour was read out
of the tree at `82af8b5`, and every number was measured on this workstation; the measurement
commands are given where they are used.

## Why this document exists

Four waves landed on `master` on 2026-09-16: `787e65d`, `a9e8ce7`, `55fa320`, `82af8b5`. The
last of them shipped a two-tier `BASE ARCHITECTURE` rail: a brand renders as a header row with
its variants nested and indented beneath it, every variant still a separately clickable,
separately counted row. The owner's response, verbatim:

> The last feature does not work as I wanted and it is terrible feature. We have added
> additional filter, despite there is already main search filter. We add subgroups with
> children, which was also not wanted. We wanted group by branding. Having only one qwen group,
> one nemotron, one lfm.

Read precisely, that is three separate rejections:

1. Grouping by brand means **consolidation**: one `QWEN` row, one `NEMOTRON` row, one `LFM` row.
   A header with its children still visible underneath is not consolidation; it is the same
   number of rows plus a header.
2. The type-to-filter box above a long rail section is unwanted and redundant with the search
   bar at the top of the screen.
3. Neither was ever confirmed with the owner before being built. The two-tier hierarchy is an
   inference made while writing `catalog-navigation-review.md` (proposal A) and hardened into a
   decision in `catalog-improvements-plan.md` (Wave 4) without being put back to the person who
   asked for brand grouping. See "The process lesson", last section.

Two further items are folded into this plan because they are what the owner raised next: the
`mmproj` files that carry no pairing signal, and a `hoard serve` that pegged sixteen cores for
eight minutes with nothing on screen to say why.

## What is on the rail today, verified

`web/js/facets.js::baseItems` builds three kinds of row out of two server lists:

- `facets.base_families` - one entry per brand holding two or more surviving base buckets, each
  rendered with `tier = " is-family"`.
- `facets.base` entries carrying `group == family.value` - rendered with `tier = " is-grouped"`,
  which `web/css/hoard.css:603` indents by 18px.
- `facets.base` entries carrying no `group` - rendered flat.

The server side: `crates/hoard-index/src/search/brands.rs` holds `BRAND_TABLE`, ten exact base
roots mapped to four brands; `crates/hoard-index/src/catalog/search/answer.rs::brand_families`
(line 625) builds one `FacetBucket` per brand holding two or more surviving buckets, counts it
with the union query `base:qwen3,qwen35moe,...`, and stamps `group` onto each member;
`crates/hoard-index/src/search/facets.rs::mark_families` marks a family active only when every
member it heads is active; `crates/hoard-server/src/api/search.rs::bucket_view` gives a family
row a `token` and `query` built from `members_of`, the comma-joined member values.

The union-not-prefix rule is the one part of the shipped design that is right and is kept: the
brand key `qwen` prefix-matches `qwenimage2512` through `Base::matches`, and `QWEN` must not
swallow Qwen-Image. Everything the union rule buys is retained below; only the second tier goes.

Two facts about the shipped table matter for the correction. From the rail measured in
`catalog-navigation-review.md` under `role:llm`, the Qwen bases present are `qwen3`, `qwen35`,
`qwen35moe`, `qwen3next`, `qwen3_tts`, `qwen3_tts_tokenizer_12hz`, `qwen3vl` - seven. The
shipped `BRAND_TABLE` names four of them. So even the two-tier rail leaves three Qwen rows
outside the `QWEN` header: an exact-root table is a maintenance treadmill that was already
behind on the day it shipped.

## Decision 1: one row per brand

### The shape

The `BASE ARCHITECTURE` section is a flat list of rows, exactly as `ROLE` and `NODE` are. There
is no second tier, no indentation, no header row, no expand affordance, and no state in which a
variant appears as its own row.

A row is one of two things and looks identical either way:

- a **brand row** - label `QWEN`, count = the union count, click = the union query;
- a **base row** - label as today, for a base value no brand claims.

Both sort together by count, descending, through the existing `order_buckets`. A brand row is
not pinned above base rows: the rail's order is "biggest first" and stays that way.

### What a click issues

Unchanged from what shipped: the union of exactly the member values the rail is holding,
`base:qwen3,qwen35,qwen35moe,qwen3next,qwen3tts,qwen3ttstokenizer12hz,qwen3vl`. Not
`base:qwen`.

The reason is the one already recorded in `catalog-improvements-plan.md` and it has not
changed: `Base::new` strips underscores, so `qwen_image_2512` normalises to `qwenimage2512`,
and `base_filter`'s bidirectional prefix rule would sweep Qwen-Image into a `base:qwen` query.
The count beside the row is that union query's own count, so a click can never land on a result
the count disagreed with, and an artifact two members both match is counted once.

The query string is long. That is acceptable: it is generated, it round-trips through the token
strip, and it is the only spelling that is true. A shorter `brand:qwen` grammar key is
explicitly **not** being added - see "What is not being built".

### Where per-variant granularity goes

Out of the rail, and nowhere new. It survives in four places that already exist:

- the table's `BASE` column, which prints every row's exact base (`web/js/table.js:75`);
- the inspector's `base` fact (`crates/hoard-server/src/api/common.rs::facts`);
- the search bar, where `base:qwen35moe` is one token and always was;
- the brand row's own tooltip.

The tooltip is the only addition, and it costs one attribute, not a row. `facetTitle` already
produces `label (value)` on hover for rows whose label and value differ. A brand row's hover
text becomes `QWEN (qwen3, qwen35, qwen35moe, qwen3next, qwen3vl)`: the user who wonders what
got merged finds out by pointing at it, and nothing is on screen until they do.

### The cases at the edges

- **A base no brand claims** stays a flat row with its exact value, exactly as today. This is
  the majority of the image side: `sdxl`, `sd15`, `sd21`, `sd3`, `flux`, `wan`, `zimage`,
  `krea2` are deliberately not in the brand table and must not be merged, for the reason
  `BASE_ALIASES` keeps them apart - an SD1.5 LoRA does not load on an SDXL checkpoint.
- **A brand with exactly one surviving member** renders as a brand row, not as the member's
  base row. This reverses the shipped behaviour (`members.len() < 2 { continue; }`,
  `answer.rs:642`) and it deletes that special case. The rationale is consistency: the rail's
  vocabulary must not flip between a brand name and an architecture string depending on how
  many variants happen to be cached this week. The union of one member is that member, so the
  count and the query are identical either way; only the label changes.
- **A member cut off by the 32-bucket cap** is not in the union and not in the count. That is
  already true of the shipped design and stays true; it is documented rather than fixed,
  because fixing it means counting bases the rail is not showing, which is what the cap exists
  to avoid.

### The brand table becomes prefix-anchored

`BRAND_TABLE` stops listing exact roots and lists **anchor roots**. A base value joins a brand
when it equals, or starts with, one of that brand's anchors. Longest anchor wins.

```
("qwen3",      "qwen",     "QWEN")
("gemma3",     "gemma",    "GEMMA")
("gemma4",     "gemma",    "GEMMA")
("lfm2",       "lfm",      "LFM")
("nemotronh",  "nemotron", "NVIDIA NEMOTRON")
```

Five entries instead of ten, and it now covers every Qwen row measured on this cache: `qwen3`,
`qwen35`, `qwen35moe`, `qwen3next`, `qwen3tts`, `qwen3ttstokenizer12hz` and `qwen3vl` all start
with `qwen3`. `qwenimage2512` does not start with `qwen3` and stays flat, so the anti-conflation
guarantee is unchanged - it is enforced by the anchor, which is specific, not by the brand key,
which is not.

The anchor rule is also what keeps the table from falling behind: a new `qwen3-omni` joins
`QWEN` on the day it appears, with no edit. A genuinely new lineage (`qwen4`) needs one line.

The existing well-formedness tests stay and one is added:

- `every_brand_anchor_is_a_normalised_base` (rename of `every_brand_root_is_a_normalised_base`);
- `no_brand_anchor_is_one_of_the_kept_separate_stable_diffusion_families` - unchanged;
- `a_brand_key_maps_to_exactly_one_label` - unchanged;
- `no_anchor_is_a_prefix_of_an_anchor_of_a_different_brand` - new, the property that makes
  "longest anchor wins" unambiguous.

### Exactly what changes

`crates/hoard-index/src/row.rs`

- `FacetBucket::group: Option<String>` - **deleted**.
- `FacetBucket::members: Vec<String>` - added. Empty on an ordinary bucket; on a brand row it
  holds the exact base values the row stands for, in rail order. This is one field replacing
  one field plus a parallel list.

`crates/hoard-index/src/search/brands.rs`

- `BRAND_TABLE` reduced to the five anchors above; the comment says anchors, not exact roots.
- `brand_of(base_root)` becomes a longest-anchor prefix match rather than an equality lookup.

`crates/hoard-index/src/search/facets.rs`

- `Facets::base_families` - **deleted**.
- `Facets::base_shown: u64` - added: how many exact base buckets the rail's rows stand for,
  which is what the cap note has to print now that rows and buckets are no longer one to one.
- `mark_families` - **deleted**. Its rule moves into `mark_active` as the `members` branch: a
  bucket with members is active when it has members and every one of them is selected; a bucket
  without members is marked as it is today. One function, two cases, no second call site.
- `Facets::mark_query` loses its `mark_families` call.

`crates/hoard-index/src/catalog/search/answer.rs`

- `FacetPass::brand_families` - **replaced** by `FacetPass::consolidate_brands(&self, excluded,
  buckets) -> Result<Vec<FacetBucket>>`, which returns the row list instead of mutating the
  bucket list and returning a parallel one. It groups surviving buckets by `brands::brand_of`,
  runs one union count per brand (same cost as today, still bounded by the table), emits a
  brand row carrying `members`, and passes through every unclaimed bucket unchanged. The
  `members.len() < 2` guard goes.
- `facets()` records `base_shown = buckets.len()` after `truncate(BASE_RAIL)`, then consolidates,
  then calls `order_buckets` a second time so brand rows sort by their union count.

`crates/hoard-server/src/api/search.rs`

- `FacetBucketView::group` - **deleted**; `FacetBucketView::members: Vec<String>` added.
- `members_of` - **deleted**; the selection a brand row's `token` and `query` are built from is
  `bucket.members.join(",")`, taken off the bucket itself.
- `FacetsView::base_families` - **deleted**; `FacetsView::base_shown: u64` added.
- The separate `view.base_families = ...` mapping pass in `facets()` - **deleted**. Every row
  now comes out of the one loop over the six dimensions.

`web/js/facets.js`

- `baseItems` - **deleted**. `sectionItems` treats `BASE ARCHITECTURE` like every other section.
- `facetRow`'s `tier` parameter - **deleted**.
- `facetText` - **deleted** (it existed only to feed the filter).
- `facetTitle` gains the member case: when `item.members` is non-empty the title is
  `` `${label} (${members.join(", ")})` ``.
- `baseCapNote` reads `facets.base_shown` instead of `(facets.base || []).length`.

`web/css/hoard.css`

- `.facet-row.is-family` (line 598) - **deleted**.
- `.facet-row.is-grouped` (line 603) - **deleted**.

Tests to rewrite, in place, against the new shape: `crates/hoard-index/src/catalog/tests/
brands.rs` (all six), the family block in `crates/hoard-index/src/search/facets.rs::tests`, the
`base_families` and `group` assertions in `crates/hoard-server/src/api/tests.rs` (around line
586) and in `api/search.rs::tests`. The replacements assert the properties that matter now:

- `a_brand_row_stands_alone_and_its_members_are_not_rows`: bases `qwen3`, `qwen35moe`,
  `qwenimage2512` present; `facets.base` holds exactly two rows, `QWEN` and `qwenimage2512`.
- `a_brand_row_unions_its_members_and_never_prefixes_them`: the `QWEN` row's query is
  `base:qwen3,qwen35moe` and `qwenimage2512` is not in it.
- `a_brand_row_counts_each_artifact_once`: an artifact based `qwen35` matches both the `qwen3`
  and `qwen35` prefix buckets; the row's count is the union's, not the sum.
- `a_brand_with_one_member_is_still_a_brand_row`: only `gemma3` present, the row reads `GEMMA`.
- `a_brand_row_is_active_only_when_its_whole_union_is_selected`.
- `a_base_no_brand_claims_is_its_own_row`.

Docs: `docs/api/hoard-api.md` - replace the `facets.base_families` paragraph with one describing
brand rows, `members` and `base_shown`. `docs/design/ui-spec.md` - replace the "one two-tier
section" paragraph (lines 127-134) with the flat-rail description and the tooltip rule.

Release asset test after this, per `CLAUDE.md`, because `web/` changes.

### What this looks like on the real cache

Under `role:llm`, the thirteen rows measured in the navigation review become four: `QWEN` (the
seven Qwen bases), `NVIDIA NEMOTRON` (two), `GEMMA` (two), `LFM` (two). Unfiltered, the rail
goes from 27 base rows to roughly 18: the same nine-row saving against a longer tail of image
architectures that no brand claims.

## Decision 2: the rail type-to-filter box is deleted

Deleted entirely. Not narrowed, not kept behind a row threshold.

Three reasons, in order of weight:

1. **The rail is for scanning, not searching.** It is a fixed, capped, count-ordered list in a
   dense layout. A text box above it turns a glanceable index into something you have to type
   at, and it competes with `/` for the same intent while doing much less: it hides rows that
   are already drawn and asks the server for nothing, so it cannot find a base the 32-bucket cap
   left off the rail, while `base:qwen3next` in the search bar can and does.
2. **It was built for a length problem that Decision 1 removes.** `FILTER_ROWS` is 15 and the
   unfiltered base section sat at 27 rows. After consolidation it sits at about 18 and the
   worst case is bounded by `BASE_RAIL`, which is a constant we control. No other section is
   near the threshold: `ROLE` is nine values, `SOURCE LAYOUT` is a closed table, `NODE` is the
   number of machines. If a section ever gets long again the honest fixes are the cap and the
   rail's own scroll, not a second search field.
3. **The owner said it is redundant with the main search filter, and they are right about the
   part that matters.** The two are not literally the same - `facets.js::railFilter` is a
   substring match over rendered labels, `search.js` commits a grammar query - but from the
   seat, both are "a text field that makes fewer things show up", and having two of them a few
   hundred pixels apart is a worse interface than having one that is strictly more capable.

Changes: delete `FILTER_ROWS`, `railFilter`, `facetText` and the `{node, text}` row bookkeeping
in `renderFacets` (`web/js/facets.js`); `renderFacets` maps items straight to rows. Delete
`input.field.rail-filter` from `web/css/hoard.css` (line 619). Delete the filter-field sentence
from `docs/design/ui-spec.md` (lines 134-138), which also disposes of the "a family and its
members filter as one" rule that Decision 1 has already made meaningless.

## Decision 3: `mmproj` becomes a projector that says it needs pairing

### What actually happens today, derived from the code and confirmed against the files

The earlier note in this session that these land as `role:clip, base:clip` is half right and the
role half is not a thing: there is no `Role::Clip` in `hoard_core::Role`. Re-derived:

Header dumps of three real files (a bounded GGUF header read, no hashing):

| file | `general.architecture` | `general.type` | `clip.projector_type` | key prefixes |
| --- | --- | --- | --- | --- |
| `mmproj-model-f16.gguf` (gemma-3-4b) | `clip` | absent | `gemma3` | `v` x437, `mm` x2 |
| `mmproj-Ministral-3-14B-...-F16.gguf` | `clip` | `mmproj` | `pixtral` | `v` x219, `mm` x4 |
| `mmproj-Qwen3.5-35B-A3B-BF16.gguf` | `clip` | `mmproj` | `qwen3vl_merger` | `v` x330, `mm` x4 |

Against `crates/hoard-index/src/infer/`:

- **Role: `unk`, confidence 0.0.** No `ROLE_RULE` in `rules.rs` matches. The nearest candidate
  is the `Role::Llm` rule, which needs `CAUSAL_LM_BLOCK_KEYS`; those are `Prefix("blk.")`, and
  a projector's keys are `v.blk.0...`, which does not start with `blk.`. `FILENAME_ROLE_HINTS`
  has no `mmproj` token. `FOLDER_ROLE_HINTS` never fires - these live at
  `<publisher>/<repo>/mmproj-*.gguf`, no `clip/` or `text_encoders/` folder in the path. So the
  accumulator is empty, `confidence >= ROLE_FLOOR` fails, and the verdict is `Role::Unk`.
- **Sub-role: none.** `pairing_sub_role` is reached only on the `role == Role::Ckpt` branch
  (`role.rs:54-58`), so the `COMPLETE` / `NEEDS PAIRING` work from Wave 1 cannot reach these
  files at all. The owner's complaint is exactly correct: there is no pairing signal.
- **Base: `clip`.** `infer_base::metadata_rules` records `general.architecture` verbatim through
  `canonical_base` at `METADATA_STRONG` (0.9). `BASE_ALIASES` has no `clip`, so the stored base
  is the literal string `clip` - which is not an architecture family anything else in the
  catalog uses, and which occupies a rail bucket of its own.
- **Consequence:** every projector on this machine sits in the triage queue (confidence 0.0 is
  far below `triage_confidence` 0.85), asking to be classified, while being one of the most
  mechanically identifiable files in the cache.

For contrast, the host beside it (`Ministral-3-14B-Reasoning-2512-Q4_K_M.gguf`) reads
`general.architecture = mistral3`, `general.type = model`, prefixes `blk`/`token_embd`, and
classifies correctly as `Role::Llm`.

### The decision

**A projector gets its own role, `Role::Proj`, it is an adapter, and it always carries
`SubRole::Bb` (`NEEDS PAIRING`).**

Rejected alternatives, briefly. Filing it under `Role::Emb` with a new `SubRole::Proj` is
cheaper - `Emb` is already in `Role::ADAPTERS`, so `compat:` would work with no enum change -
but a row reading `EMB` for a vision tower is a lie told to save an enum variant, and the whole
point of the change is that the rail and the table should say what a file is. Filing it under
`Role::Ckpt` with `SubRole::Bb` would be semantically close ("part of a model, needs its other
half") but `Ckpt` is in `Role::HOSTS`, so `compat:` would start offering vision projectors as
hosts for LoRAs. Leaving it `unk` is the status quo the owner rejected.

`Role::Proj` joins `Role::ADAPTERS`, which is what makes the pairing actionable rather than
decorative: `api/compat.rs::direction` gives an adapter the `AdapterResolvesAgainstHost` rule,
so the inspector's `PLUGS INTO` panel lists the host LLMs the projector's base matches, with no
new machinery.

`SubRole::Bb`'s label is already `NEEDS PAIRING`, and keeping it on every projector is
deliberate: `role:needs-pairing` then answers with both kinds of half-model in one query - the
bare diffusion backbone and the vision projector - which is the category the owner described
("do not have any sense on their own"). This requires `SubRole::role() -> Role` to become
`SubRole::roles() -> &'static [Role]` (`Bb` and `Cmp` map to `[Ckpt, Proj]` and `[Ckpt]`
respectively), with two call sites following: the `sub.role() == role` filter in
`infer/role.rs:53` and the `sub.role().as_str() == value` filter in `catalog/query.rs:279`.

### The detection rule

A new `projector_rule(input, tensors, roles, sub_roles)` in `crates/hoard-index/src/infer/
role.rs`, called from `infer_role` beside `architecture_rule`, firing on the first match:

1. `metadata["general.type"] == "mmproj"` - `METADATA_STRONG` (0.9). Authoritative; the writer
   said so.
2. `metadata["general.architecture"] == "clip"` and `metadata["clip.has_vision_encoder"]` is
   truthy - `METADATA_STRONG`. Covers the older files that predate `general.type`.
3. `KeyMatch::Segment("mm")` and `KeyMatch::Segment("v")` both present, with no
   `CAUSAL_LM_EMBED_KEYS` - `TENSOR_PATTERN` (0.9). The shape rule, for a file whose metadata
   was stripped.

Each emits `Role::Proj` and `SubRole::Bb`. Any one of them clears `ROLE_FLOOR` (0.5) on its own,
so a projector leaves the triage queue. `("mmproj", Role::Proj, Some(SubRole::Bb))` is added to
`FILENAME_ROLE_HINTS` as the usual weak corroboration at `FILENAME_HINT` (0.4), which on its own
does not clear the floor - consistent with every other filename hint.

### The base

`infer_base::metadata_rules` gains one branch, guarded on the file being a projector:

- Do **not** record `general.architecture` when it is `clip`. It names the file's own format,
  not a family, and it is the sole source of the junk `clip` bucket on the rail.
- Record `clip.projector_type` instead, normalised through `canonical_base` with a trailing
  `merger`, `projector` or `adapter` trimmed after normalisation, and skipped entirely when the
  value is one of the generic projector shapes (`mlp`, `mlp_norm`, `ldp`, `ldpv2`, `resampler`),
  which name a topology rather than a family.

Measured against the three real files: `gemma3` -> `gemma3`, which is exactly its host's base;
`qwen3vl_merger` -> `qwen3vl`, exactly its host's base; `pixtral` -> `pixtral`, which is the
true vision family but not the host's `mistral3`, so that projector's `PLUGS INTO` panel will be
empty while its row still reads `NEEDS PAIRING`. Two of three resolve to their host for one line
of rule, and the third is honest rather than wrong.

A stronger pairing exists and is deliberately not taken here: a projector's host is the sibling
model in its own repository, which `discover/loose.rs` already keeps together ("the model and
its projector travel together"). Taking it would need revision-level assembly, and
`catalog/scan/mod.rs` prepares and commits one **unit** at a time (a projector is its own unit),
so a sibling's verdict is not reliably available when the projector's unit is stored. That is a
separate piece of work with a real design question in it; it is named in "What is not being
built" rather than smuggled in here.

### Cost

This changes a stored role and a stored base, so it is a `catalog::FORMAT_VERSION` bump, per
the Compatibility section of `CLAUDE.md`. Read Decision 4 before scheduling it: a bump currently
costs a full re-read of every byte in the cache, and this plan makes that cheap before it makes
it necessary.

Surfaces that follow a new `Role` variant, all compiler-guided because the workspace forbids
catch-all matches: `Role::as_str`/`code`/`label`/`aliases`/`from_alias`/`ADAPTERS` in
`crates/hoard-core/src/role.rs`, the role match in `api/common.rs::facts`, the `--role-proj`
token and `.r-proj` rule in `web/css/hoard.css` (both currently alias `--role-unk`, so this is
two lines), the role table in `docs/api/hoard-api.md` and the role list in
`docs/design/ui-spec.md`.

## Decision 4: the hashing pass

### What was observed

A `hoard serve` built from `82af8b5` held 1500-1600% CPU for more than eight minutes. Its own
`GET /api/v1/jobs` reported a scan job at phase `hashing`, `progress` 0.0, `bytes` 0,
`rate_bps` null, for the whole window. Its open file descriptors held several 20-40 GB GGUF
shards at once (a 120B Nemotron split across shards, a 35B Qwen, others). It was stopped with
`SIGTERM` rather than left to finish, so there is no completion time to quote.

### What the code says: three findings, only one of which is a bug

**Finding 1 - the progress callback is thrown away. This is a real bug.**

`crates/hoard-index/src/coordinate.rs:334` calls
`catalog.hash_pending_until(config, stop, |_| {})`. The closure discards every report. The job
row is written once, by `phase(catalog, item.job, "hashing")` at line 332, whose `JobProgress`
is `JobProgress::default()` - so `progress: 0.0`, `bytes: 0`, `total: None`, `rate_bps: None`
are written deliberately, at the start, and never touched again. The walk phase before it is
just as silent: `rescan_until(..., |_, _| {})` at line 329.

So `progress 0.0, bytes 0, rate_bps null` is not evidence of a stall. It is the only reading
this code can produce, at any speed, for any cache. `docs/api/hoard-api.md` line 216 documents
`phase`, `progress` (0..1), `bytes` and `rate_bps` for scans; for a scan, three of those four
are fiction today. The CLI is better but not good: `src/cli/scan.rs:80` prints
`Hashing: {count} files`, a count of **completed** files.

Even the callback that exists cannot move during a large file. `read_digests` calls
`progress(hashed.load(...))` only inside the paging loop, right after `work_tx.send(pair)`
(`catalog/hashing.rs:191`), and that channel has capacity `workers`. Once every worker is busy
the paging thread blocks in `send` and no progress call happens until a file **finishes**. With
twenty workers on twenty multi-gigabyte shards, that is minutes of silence by construction.

**Finding 2 - the worker count is unbounded by anything but the core count. This is a design
flaw, not a bug.**

`IndexConfig::hash_workers()` (`config.rs:177`) returns `rayon::current_num_threads()` when
`hash_threads` is 0, which is the default and what `hoard.toml` uses. This box has 20 cores, so
the pass spawns 20 std threads, each streaming a file in 1 MiB buffers through sha256. The
observed 1500-1600% is exactly that. `hash_many` in `hash/mod.rs:191` uses a bare rayon
`par_iter` with no bound, but it is not on this path - the scan pass has its own thread scope.

`HashPolicy::Background` is documented in `config.rs:15` as "Hash in a background rayon pool at
the lowest priority". There is no such pool and no priority call anywhere in the workspace;
`coordinate.rs::run_scan` treats `Now` and `Background` identically. The comment is untrue.

**Finding 3 - the full rehash was self-inflicted and was foreseeable.**

Two of the four waves that landed today bumped `catalog::FORMAT_VERSION`, 8 -> 9 -> 10.
`Catalog::open_or_scan` deletes the catalog directory on a format mismatch and rescans from
disk. Every entry therefore loses its `sha256`, `owes_a_hash` is true for all of them, and the
whole cache - 314 GB of hf-cache plus the LM Studio and ComfyUI trees - is re-read end to end.
That is the Compatibility policy working as written, and it is the right policy. But nothing
warned the owner that this delivery would cost a full rehash, and nothing on screen said it was
happening.

There is a deeper point here. A content digest is a function of bytes. A `FORMAT_VERSION` bump
is a statement about *inference rules*. Throwing away every sha256 because an inference rule
changed discards the one derived value that was provably still correct, and it is why a
one-line tweak to a tensor rule costs hours of disk and CPU.

### The measurement: the debug build is a 20x factor on its own

`sha2` 0.11, this machine (aarch64, 20 cores), one core, 512 MiB through one hasher:

| build | throughput |
| --- | --- |
| debug, default profile | 95.7 MB/s |
| debug, `[profile.dev.package.sha2] opt-level = 3` | 1868.7 MB/s |
| release | 2037.0 MB/s |

(Measured with a three-line `main` in a scratch crate against the same `sha2` version the
workspace pins; the release and optimised-debug figures agree to within 9%, which is what tells
you the gap is optimisation of the hashing crate and nothing else.)

A debug `cargo run -- serve` therefore spends about **twenty times** the CPU-seconds of a
release binary to hash the same bytes. Twenty cores at 96 MB/s is 1.9 GB/s of aggregate
hashing, which is roughly what a debug build needs to keep a modern NVMe busy - so the machine
converts a disk-bound job into a CPU-bound, heat-producing one. At release speed, four to eight
workers saturate the same device and the rest of the cores stay idle.

### Verdict

**Not stuck. Slow by design, twenty times slower than it needed to be because of the build, and
completely silent because of a real reporting bug.** Each of the three observations is fully
explained: 1500-1600% CPU = 20 debug-speed workers; no progress = the callback is discarded and
could not have moved anyway mid-file; huge files held open = exactly the work it was asked to
do after a format bump erased every digest.

### What to change, in order

**H1 - report progress during hashing. (The bug. Do this first; it is the only item the owner
sees directly.)**

- `content_digests` / `content_hashes` take a `&AtomicU64` the read loop adds each buffer to,
  beside the `&AtomicBool` stop flag it already reads there. One extra relaxed add per MiB.
- `hash_pending_until` takes `impl FnMut(HashProgress)` where `HashProgress` is
  `{ files_done, files_total, bytes_done, bytes_total }`, and runs a small ticker thread inside
  its existing `thread::scope` that samples the atomics every 500 ms and calls it. Time-based
  sampling, not event-based, is the whole point: a 40 GB file moves the bar and `rate_bps` is a
  difference between two samples.
- `coordinate.rs::run_scan` passes a closure that writes a real `JobProgress`
  (`progress = bytes_done / bytes_total`, `bytes`, `total`, `rate_bps`) instead of `|_| {}`, and
  does the same for the walk phase's `(phase, count)` callback.
- `bytes_total` comes from a `pending_bytes` counter on `Source`, maintained beside the existing
  `pending_hashes` by the same writers and moved through `scan::DerivedShift` per the rule in
  `CLAUDE.md`. It is also shown per source in `SourceView` (`api/jobs.rs`) and the sources table
  (`web/js/jobs.js:54`), so "this machine owes 214 GB of hashing" is legible before anyone opens
  the jobs tab. Stored record change: bump.
- `src/cli/scan.rs` prints bytes and a rate, not a file count.
- Tests: `progress_reports_every_byte_once` already exists for `sha256_file_with_progress`; add
  the equivalent for `content_digests`' counter, and a `hash_pending_until` test asserting the
  callback fires at least twice while one multi-buffer file is being read.

**H2 - bound the workers, and make `background` mean something.**

- `hash_workers()` with `hash_threads = 0` returns `(rayon::current_num_threads() / 4).clamp(2, 8)`
  instead of the full core count. Five workers on this box. Streaming sha256 at release speed is
  ~2 GB/s per core; two to eight concurrent streams keep any NVMe queue full, and more only buys
  cache thrash, page-cache eviction and heat. An operator who wants all twenty sets
  `hash_threads` explicitly, which is what the knob is for.
- Fix the `HashPolicy::Background` doc comment to say what it does. If the two policies should
  genuinely differ in pressure, the cheapest honest version is `Background` using the capped
  count and `Now` using `hash_threads` or the full pool; thread priority via the
  `thread-priority` crate is a possible follow-on, not part of this, because the cap does the
  work and the workspace forbids `unsafe` so a bare `setpriority` is not available anyway.
- Document `hash_threads` in `hoard.toml` with the default spelled out.

**H3 - stop paying for a rehash the bytes did not earn.**

A sidecar digest memo at `<data_dir>/digests.redb`, keyed by `FileStamp` (inode, mtime, size -
the identical key `decide_unit` already trusts to skip a file within a catalog's life), valued
by `(sha256, git_sha1)`. Written whenever a digest is computed; consulted by `pending_batch`
before a file is queued. It carries its own `DIGEST_CACHE_VERSION` and is deleted whole on a
mismatch or on any read error.

This does not soften the Compatibility policy and is not a version ladder: the catalog is still
deleted and rebuilt from disk on a `FORMAT_VERSION` change, exactly as `CLAUDE.md` requires.
The memo is not the catalog; it is a stamp-to-digest cache that happens to survive, and it has
one shape like everything else. The effect is that a future inference change costs a header
re-sniff (megabytes) instead of a full content read (hundreds of gigabytes).

If the owner would rather not carry a second on-disk file, then say so plainly and stop: the
full rehash after a bump is by design, H1 and H2 and H4 make it visible and bounded, and every
future inference tweak costs hours. That is a legitimate choice, but it should be a choice.

**H4 - stop hashing at debug speed.**

- Add to the root `Cargo.toml`:
  ```toml
  [profile.dev.package.sha2]
  opt-level = 3
  [profile.dev.package.sha1]
  opt-level = 3
  [profile.dev.package.xxhash-rust]
  opt-level = 3
  ```
  Measured effect: 95.7 -> 1868.7 MB/s per core in a debug build, with hoard's own crates still
  unoptimised and still debuggable. Three stanzas, no code risk, a 19.5x reduction in the CPU
  cost of the thing that was pegging the machine.
- Record in `docs/handoff.md` that any run against the real caches uses
  `cargo run --release -- serve`, and that a debug serve is for the request path, not for a
  first-run scan.

**H5 - say what the rebuild is for.**

`open_or_scan` already logs a warning when it deletes an unreadable catalog. Extend it to name
the reason (`format 8 on disk, this build writes 10`) and carry that onto the resulting scan
job's `subject`, so the jobs tab reads `rebuilding after a catalog format change` rather than an
anonymous scan. This costs nothing and is what would have made the eight minutes explicable
while they were happening.

### Sequencing

H4 and H1 first, in that order, because together they turn "twenty silent pegged cores" into "a
visible, bounded job" without touching a stored record. H2 with them; it is a constant. H3 next,
because it is what makes the bump in Decision 3 cheap. Only then Decision 3, which bumps
`FORMAT_VERSION` and forces the rescan that H3 has by then made affordable. Decisions 1 and 2
touch no stored record and no schema and can land at any point; they are the owner's stated
priority, so they should go first in wall-clock order even though they are last in this
dependency chain.

One `FORMAT_VERSION` bump for the whole delivery: H1's `pending_bytes` counter and Decision 3's
role and base changes are both stored-record changes, so they land in the same wave and bump
once, not twice. Whoever ships it tells the owner, before it lands, what the first run after it
will cost.

## What is not being built

- **A `brand:` or `family:` grammar key.** A brand is a rail presentation, and giving it a query
  key makes it a stored concept people will expect `compat:` to honour. The union query is the
  spelling, and it is generated.
- **Expanding a brand row on click or on selection.** That is the hierarchy, arriving by a
  different door.
- **A narrower rail filter.** See Decision 2.
- **Pairing a projector to its sibling host by repository.** The right answer, and out of scope:
  units are prepared and committed independently in `catalog/scan/mod.rs`, so it needs a
  revision-level assembly step that does not exist yet. Revisit once Decision 3 has landed and
  the `PLUGS INTO` panel shows how often the `projector_type` base is enough on its own.
- **Hand-written human labels per base value**, the `PUBLISHER` facet, and the cards-view brand
  signal - still deferred, unchanged from `catalog-improvements-plan.md`.

## The process lesson

The two-tier rail was never what the owner asked for. They asked for grouping by brand; the
review document turned that into a brand-header-with-variant-rows proposal, the implementation
plan promoted it to a decision, a wave plan specified it, an implementer built it and a reviewer
passed it - and at no point between the original request and the merge did anyone put the actual
shape back to the person who asked, in one sentence, and wait for a yes. Every round after the
first was reviewing conformance to an inference, not to a requirement, which is exactly the
failure mode that a long chain of careful reviews cannot catch. The rule this earns: when a
request is about how something should *look*, the design gets confirmed with the person before a
plan is written from it, and a review document's proposal is not a confirmation.

The second half is cheaper to fix and was just as avoidable. Two `catalog::FORMAT_VERSION` bumps
went out in one delivery window. The policy that makes a bump a full rebuild is correct and was
followed; what was missing is that a bump has a user-visible cost - here, re-reading every byte
of a 300 GB cache on a debug binary with no progress on screen - and nobody said so in advance.
A wave that bumps the format says what the first run after it will cost, in the delivery note,
before it lands.
