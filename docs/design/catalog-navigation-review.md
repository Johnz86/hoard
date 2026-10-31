# Catalog navigation: UX and information architecture review

Historical review. Its proposals were revised and implemented by later work; see
[current design status](README.md) before using this document as a backlog.

Status: proposal only. No application code was changed to produce this document. Method,
findings and proposals below; nothing here is authorized for implementation as written.

## Method

The local server was started against the real configured sources (`hoard.toml`, the
workstation's actual HuggingFace cache, ComfyUI models and LM Studio models) rather than
synthetic fixtures: 235 artifacts, 103 repos, 25 in triage, 1.6 TB indexed. The UI was driven
headless (`brave-browser --headless=new`) at 1600x1000 against `http://127.0.0.1:7099`, deep
linked through the app's hash routes (`#/index?q=...`) to capture the index view unfiltered,
filtered by role and by base architecture, a repo detail page, and the cards view. Screenshots
are under `docs/design/evidence/catalog-navigation/`, referenced by filename below. Facet and
row data were also pulled directly from `/api/v1/search` and `/api/v1/stats` to get exact
counts, and the relevant Rust (`hoard-index`'s `infer/base.rs`, `catalog/search/tally.rs`,
`catalog/query.rs`, `search/facets.rs`) and JS (`web/js/facets.js`) were read to describe root
causes precisely rather than guess at them.

## What "navigation drawer" maps to in this build

Hoard has no hamburger-style navigation drawer. The two structures a user browses categories
through are the top tab bar (`INDEX / SHELVES / NODES / SCANS / JOBS`) and the **facet rail**
on the left of the index view — `ROLE`, `BASE ARCHITECTURE`, `FORMAT · PRECISION`, `NODE`, then
a `SAVED SHELVES` list (`docs/design/ui-spec.md`, `web/js/facets.js`). The rail's
`BASE ARCHITECTURE` section is where per-model "categories" live today, and it is what this
review treats as the subject of the request — it is the only place the catalog groups models
into something resembling a browsable taxonomy.

![Full index view, unfiltered, facet rail visible](evidence/catalog-navigation/01-index-facet-rail.png)

## Finding 1: the BASE ARCHITECTURE rail fragments one brand into many rows

This is the behavior the request named directly (LFM-2.5 / LFM-2.5-moe, Qwen). It reproduces
exactly against the real cache. Querying `role:llm` (58 artifacts) and reading the `base` rail:

| Rail row (`value`) | Count | What it actually is |
| --- | --- | --- |
| `qwen3` | 11 | Qwen3 dense |
| `qwen35` | 7 | Qwen3.5 dense |
| `qwen35moe` | 7 | Qwen3.5 MoE |
| `qwen3next` | 5 | Qwen3-Coder-Next |
| `qwen3_tts` | 4 | Qwen3 TTS |
| `qwen3_tts_tokenizer_12hz` | 5 | Qwen3 TTS tokenizer |
| `qwen3vl` | 4 | Qwen3-VL |
| `nemotron_h` | 5 | NVIDIA Nemotron dense |
| `nemotron_h_moe` | 5 | NVIDIA Nemotron MoE |
| `gemma3` | 3 | Gemma 3 |
| `gemma4` | 3 | Gemma 4 |
| `lfm2` | 2 | LFM2 dense |
| `lfm2moe` | 2 | LFM2 MoE |

Qwen alone occupies **7 separate rail rows** for one brand. A user scanning the rail to find
"the Qwen models" has to already know every spelling variant exists and mentally merge them;
nothing in the rail suggests they are related. The full unfiltered rail (`role:` not set) adds
`sdxl`, `sd15`/`sd21`/`sd3`/`flux`/`wan`/`zimage`/`krea2` from the image side and shows the same
pattern is not LLM-specific.

![role:llm, base rail expanded, showing the Qwen/Gemma/Nemotron/LFM fragmentation by name](evidence/catalog-navigation/02-index-role-llm.png)

### Root cause, precisely

This is a presentation gap, not a missing capability — the catalog already has the relationship
the rail fails to show:

- `infer_base` (`crates/hoard-index/src/infer/base.rs`) records the **exact** architecture
  string from `model_type` / `architectures` / GGUF `general.architecture`, on purpose: it is
  what the compatibility engine (`compat:`) matches on, and Qwen3 dense, Qwen3.5-MoE and
  Qwen3-VL are not weight-compatible with each other, so collapsing them at inference time
  would be a correctness bug, not a UX fix.
- The query engine already resolves a **family relationship** between these exact strings.
  `base_filter` (`crates/hoard-index/src/catalog/query.rs`) matches a `base:` token
  bidirectionally: a stored value that starts with the query, *and* a stored value the query
  starts with. Typing `base:qwen3` currently returns all 12 Qwen3-prefixed artifacts —
  `qwen3`, `qwen35`, `qwen35moe`, `qwen3next`, `qwen3vl`, `qwen3_tts*` — in one query, because
  every one of those exact strings happens to share the literal prefix `qwen3`.
- `base_candidates` (`crates/hoard-index/src/catalog/search/tally.rs`) already computes,
  for the 32-bucket cap, which exact values are "family roots" that other buckets extend, so a
  root doesn't get pushed off the rail by unrelated high-count siblings.
- None of that reaches the rail as structure. `renderFacets` in `web/js/facets.js` takes
  whatever `facets.base` returns and flat-maps it into one row per exact value, sorted by count
  (`order_buckets` in `tally.rs`). The family relationship is real and already computed for
  query matching; the rail throws it away and shows 7 siblings as 7 unrelated categories.

![base:qwen3 typed directly: 12 artifacts, 7 different exact base values, confirming the family relation already exists in search but not in the rail](evidence/catalog-navigation/03-base-qwen3-prefix-match.png)

### Why this isn't as simple as "always merge by prefix"

Stable Diffusion is the counter-example already encoded correctly in the codebase:
`BASE_ALIASES` (`crates/hoard-index/src/infer/rules.rs`) normalizes spelling variants onto
`sd15`, `sd21`, `sd3`, `sdxl` — and deliberately keeps those **four as separate families**,
because an SD1.5 LoRA does not load on an SDXL checkpoint. If a fix here merged by literal
string prefix alone, `sd15`/`sd21` don't even share one (no false merge risk there by luck of
spelling), but a naive "first N characters" heuristic applied elsewhere could just as easily
merge things that must stay apart. The Qwen/Gemma/Nemotron/LFM case is different in kind from
the SD case: those variants *are* one product lineage from one publisher that a user reasonably
wants to browse as one entry, weight-compatibility notwithstanding. The distinction the IA needs
is:

- **Architecture / `base`** — compat-relevant, must stay exact, never collapsed. This is what
  `compat:` and the PLUGS INTO panel depend on.
- **Brand / family** — purely orientational, safe to group in the rail, must never touch the
  stored `base` value or the compat query.

Gemma is the case that shows plain lexical prefixing ("share a literal string prefix") is not
sufficient even for the grouping-only version of this: `gemma3` and `gemma4` do **not** share a
prefix beyond `gemma`, and a 5-character floor is too short to trust in general (it would also
merge unrelated things elsewhere in a large catalog). Brand grouping needs its own small alias
table in the spirit of `BASE_ALIASES` — mapping a base root to a human brand label — rather than
leaning on string-prefix coincidence the way the query engine currently does for matching.

## Finding 2: org is missing for most of the catalog, so "group by publisher" can't be the whole answer

Pulling every row currently indexed: **205 of 233 artifacts (88%) carry no `org`.** These are
GGUF conversions and loose ComfyUI/LM Studio files with no HuggingFace repo metadata at all —
`NVIDIA-Nemotron-3-Nano-4B-Q8_0.gguf`, `Qwen3.5-35B-A3B-Q8_0.gguf`, `4x-UltraSharp.pth`, and
so on render with a blank org cell in the NAME column today (visible in every screenshot above).
This matters directly for the brand-grouping proposal: it cannot be `org`-keyed as the primary
signal the way a pure HuggingFace-Hub browser could be, because on a real workstation-class node
most of the catalog simply doesn't have one. Brand has to be derivable from the architecture
string (which is present far more often) or the filename, with `org` used only where it exists,
as corroboration or override.

Also from the same pull: **100 of 233 artifacts (43%) carry no `base` at all** — upscalers with
architecture-agnostic weights, unrecognized safetensors dumps, VAEs. Any design that routes
"unbranded" models into a dead end rather than a first-class, visible bucket will make close to
half of a catalog like this one harder to reach than it is today, not easier.

## Finding 3: rail rows are terse tokens, not labels, and get silently truncated

`qwen3_tts_tokenizer_12hz` renders as `qwen3_tts_tokeniz…` in the rail at its fixed width, with
no `title` attribute to recover the full string on hover (`facetRow` in `web/js/facets.js`
never sets one). Comparing adjacent rows — `qwen3_tts` vs `qwen3_tts_tokeniz…`, `nemotron_h` vs
`nemotron_h_moe` — a user has to decode near-identical, truncated, lowercase-with-underscores
tokens to tell whether two rows are the same thing spelled differently or genuinely distinct.
This compounds Finding 1: even where rows *are* legitimately distinct, the presentation makes
them look like arbitrary noise rather than "the MoE variant of the row above it." A `label`
field already exists per bucket (`FacetBucket`, populated in some cases from evidence, e.g.
`nemotronh` → label `nemotron_h`) but is not used to produce anything more readable than the
value itself for most rows, and `facetRow` falls back to `item.value` when no label is set.

## Finding 4: the 32-bucket cap is silent

`docs/api/hoard-api.md` documents that `facets.base` returns at most 32 buckets and "a base
outside the 32 is not shown and is not marked active." At 235 artifacts the real rail already
sits at 27 distinct base values for the unfiltered query — close enough to the cap to be worth
flagging now, before the catalog grows toward the 1M-artifact scale `docs/native-catalog.md`
measures against. Today, hitting the cap means a real architecture silently disappears from the
rail with no "27 shown, N more not shown" affordance and no way to search within the facet to
reach it. A query for it by hand still works (the cap is a rail-rendering limit, not a query
limit), but nothing in the UI tells a browsing user that option exists.

## Finding 5: triage is a real fraction of the catalog and easy to miss

25 of 235 artifacts (about 11%) sit in `role:?` (unclassified/triage) right now. The only way
to reach them from the index view is the `triage queue` row inside `SAVED SHELVES`, a plain-text
link at the bottom of a rail that, per Finding 4, is already getting crowded — it carries no
count, no visual weight, and is easy to scroll past. A new or returning user has no way to know
11% of what's indexed needs attention without already knowing to look for that specific shelf.

## Finding 6: adapters show no base signal at all in Cards view

The CARDS view (LoRA identity, per `docs/design/ui-spec.md`) renders `base` as a plain dash
(`–`) for artifacts with no inferred base — visible on essentially every LoRA card in the real
catalog, since LoRAs frequently don't carry standalone architecture metadata the way checkpoints
do. This isn't wrong given the data, but it means the brand-fragmentation problem in Finding 1
has a mirror-image problem in Cards view: adapters, which are the view CARDS is designed for
(per the spec), currently carry no brand/family signal to browse by at all.

![Cards view: base column reads "–" for most LoRA cards](evidence/catalog-navigation/05-cards-view.png)

## Design constraint that has to hold across every proposal below

**Grouping is a rail/rendering concern only. It must never change what `base` an artifact
carries or what `compat:` matches on.** The exact base string is load-bearing: it's what
`base_compat` in `catalog/query.rs` uses to decide whether a LoRA plugs into a checkpoint, and
what the PLUGS INTO panel in the inspector renders from. Every proposal below adds a
presentation-only "family" or "brand" label above the existing exact value; none of them touch
`infer_base`, the stored `base` field, or the compat rule.

## Proposals

### A. Two-tier BASE ARCHITECTURE rail: brand header, variant rows beneath it (primary recommendation)

Replace the flat, count-sorted list of exact `base` values with a small curated **brand alias
table** (same shape as `BASE_ALIASES`, but mapping a base root to a human brand label instead of
canonicalizing spelling — e.g. `qwen* → "Qwen"`, `gemma* → "Gemma"`, `lfm2* → "LFM"`,
`nemotron_h* → "NVIDIA Nemotron"`) and render the rail as:

```
BASE ARCHITECTURE
  QWEN                              33
    qwen3                     11
    qwen3.5                    7
    qwen3.5 moe                7
    qwen3-vl                   4
    qwen3-coder-next            5
    qwen3 tts (+tokenizer)      9
  GEMMA                              6
    gemma3                      3
    gemma4                      3
  NEMOTRON                          10
    nemotron_h                  5
    nemotron_h moe               5
  SDXL                               2
  SD 1.5                             -
  FLUX                              10
```

Clicking a brand header issues the broad query (`base:qwen`, using the existing bidirectional
prefix match — no new query grammar needed). Clicking a variant row issues the existing exact
query. Brands with only one variant (`sdxl`, `flux`, `wan`) render as a single row exactly as
today — the hierarchy only appears where it resolves something. This uses infrastructure that
already exists (`base_filter`'s prefix matching, `base_candidates`' root detection) and adds one
small static table plus a grouping step before `renderFacets` flat-maps buckets into rows.

### B. A PUBLISHER facet, separate from BASE ARCHITECTURE, populated from `org`

Given Finding 2 (88% of this catalog has no `org`), publisher cannot replace architecture-based
grouping, but it's a real, currently-absent facet for the 12% of artifacts that do carry one, and
it's already computed and shown inline in the NAME column. Add it as its own rail section with
an explicit `UNKNOWN PUBLISHER` bucket rather than omitting orgless artifacts from a publisher
view entirely — the bucket itself is useful information (it says "locally converted / hand
placed," which is a real distinction a self-hosted mirror's users care about).

### C. Real labels, no silent truncation

Populate `FacetBucket.label` for every base value the brand table or `infer_base`'s evidence can
produce a human string for (`qwen3_tts_tokenizer_12hz` → "Qwen3 TTS Tokenizer (12 kHz)"), and
add a `title` attribute carrying the full untruncated label/value on every facet row so a user
can always recover what a clipped row actually says.

### D. Rail scale affordances

Two independent, small additions: show `"27 shown"` vs `"27 of 41 — showing the most common"`
when `base_candidates` truncates at the 32 cap, and add a lightweight type-to-filter box above a
facet section once it exceeds a threshold (say 15 rows) so a long list is searchable instead of
requiring a scroll-and-scan. Both are aimed squarely at the 1M-artifact scale the catalog is
being measured against, not at the 235-artifact node reviewed here — but the rail is already at
27/32 on real hardware today, so this is closer than it looks.

### E. Surface triage count persistently

Give the `role:?` count a fixed, visible presence instead of only a buried shelf link — either a
small badge next to the `SAVED SHELVES` label reading the live triage count, or promoting
`triage queue` out of the plain shelf list into its own short line under `ROLE` (it already has
a role value, `unk`/`?`, so it fits naturally next to the other role rows rather than only inside
a saved-query shortcut).

## Non-goals / open questions for a follow-up design pass

- The brand alias table's exact contents (which roots map to which label) is editorial work
  this review didn't attempt — it needs a pass over the actual architecture vocabulary
  `infer_base` produces, ideally cross-checked against a larger, more varied cache than the one
  measured here.
- Whether brand headers belong only in the rail, or also as a `brand:`/`family:` query token
  users can type directly, is a query-grammar decision (`docs/api/hoard-api.md`) out of scope
  for a rail-rendering review.
- Cards view (Finding 6) showing no brand signal for adapters may want a different fix entirely
  (e.g. surfacing the LoRA's declared/inferred compat target instead of `base`) — flagged here,
  not designed here.
