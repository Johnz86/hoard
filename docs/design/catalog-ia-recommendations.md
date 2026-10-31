# Catalog IA recommendations: folding ComfyUI's taxonomy into hoard's

Historical proposal. Later decisions supersede parts of this document; see
[current design status](README.md) for the implementation and decision order.

Status: proposal only. No application code was changed to produce this document. Builds on
`docs/design/catalog-navigation-review.md` (the brand-fragmentation review) and
`docs/design/comfyui-model-taxonomy.md` (the ComfyUI folder-mechanics research); read both
first. This document does not repeat their findings at length — it answers the questions they
raised: how hoard's taxonomy should evolve, how the pieces compose into one rail, and what that
rail looks like.

## The one-sentence thesis

ComfyUI's taxonomy is a placement decision the user already made; hoard's taxonomy is a content
guess layered on top. They should not compete for the same rail section — placement becomes its
own facet, sitting next to the content-inferred ones, so a wrong or absent content guess never
hides what the user already told the filesystem.

## Recommendation 1 (highest priority): a SOURCE LAYOUT facet, built from data hoard already stores

Add a new rail section, `SOURCE LAYOUT`, populated from the artifact's own `path` for
loose-kind sources: the ComfyUI folder it was found under (`checkpoints`, `diffusion_models`
— with `unet` folded in, since ComfyUI itself treats them as one — `text_encoders` — with
`clip` folded in — `loras`, `controlnet`, `vae`, `upscale_models`, ...), read the same way
`FOLDER_ROLE_HINTS` already reads it in `crates/hoard-index/src/infer/rules.rs`. hf-cache-sourced
artifacts get a single `hub repo` value, or no value at all — they don't have a ComfyUI-meaningful
folder, and their identity signal is `org`/`repo`, which Recommendation 3 covers separately.

This is presentation, not classification, which is what makes it cheap and safe:

- It does **not** touch `FOLDER_HINT`'s 0.35-against-a-0.5-floor policy
  (`crates/hoard-index/src/infer/rules.rs`). That floor exists to keep a folder name from
  silently overriding a tensor-sniffed role, and it should stay exactly as conservative as it
  is for that job. `SOURCE LAYOUT` isn't the role; it's a second, always-visible fact, so it
  carries no risk of a wrong folder guess corrupting `compat:` or the stored `base`/`role`.
- It resolves the two open triage cases from the taxonomy doc for free. `checkpoints/anima/`,
  `hunyuan_3d_v2.1.safetensors`, and `diffusion_models/pixal3d_*` sit in `role:unk` because no
  tensor rule recognizes their key namespace — but a user scanning the rail sees
  `SOURCE LAYOUT: checkpoints (3)` and `SOURCE LAYOUT: diffusion_models (2)` sitting right next
  to `ROLE: unk (5)` and immediately has the answer the folder hint already knew, without
  hoard ever having to trust that hint enough to auto-classify on it.
- It resolves the `text_encoders/` role split (the taxonomy doc's sharpest finding) without
  touching the role inference that causes it. Whether `qwen_3_4b.safetensors` lands in
  `role:llm` or `role:ckpt` stops mattering for browsing once `SOURCE LAYOUT: text_encoders`
  groups it with the other 12 regardless of role — the inconsistency becomes a harmless
  implementation detail instead of a user-facing one. This is deliberately **not** a proposal
  to fix the role split at the inference level: the split tracks a real architectural fact
  (encoder-decoder vs. decoder-only), and collapsing it would lose that fact for the users who
  do want it (`role:llm` should keep meaning "the tensors are a language model," full stop).
  `SOURCE LAYOUT` is the axis that answers "what does ComfyUI do with this," which is a
  different question with a different, folder-shaped answer.
- It directly disambiguates the HiDream case: `checkpoints/hidream_o1_image_dev_fp8_scaled.safetensors`
  keeps its (wrong) `role:llm`, but `SOURCE LAYOUT: checkpoints` sits right there in the rail
  and the inspector, which is enough for a user who already knows their own cache to not be
  misled by the role alone.

## Recommendation 2: a COMPLETENESS tag, built from evidence hoard already computes

`checkpoints/` (self-contained: `MODEL, CLIP, VAE` in one file) vs. `diffusion_models/`
(backbone only, needs a separately-loaded CLIP and VAE to run) is the distinction ComfyUI users
weigh constantly — "can I use this right now, or do I still need to go find two more files" —
and hoard's single `ckpt` role currently erases it. It doesn't need new inference: `ROLE_RULES`
in `crates/hoard-index/src/infer/rules.rs` already distinguishes these two cases as separate
rule entries with different evidence:

```
detail: "diffusion model keys with text encoder keys"   // complete: TEXT_ENCODER_KEYS present too
detail: "a diffusion model group holding the whole file" // backbone only: no text encoder keys
```

`GET /api/v1/artifacts/{id}` already returns this as `evidence[].detail`
(`docs/api/hoard-api.md`). The gap is purely that it stops at the inspector's evidence list and
never becomes a value anything can filter or facet on. Promote it: when the winning `Role::Ckpt`
evidence detail is the "with text encoder keys" variant, tag the artifact `COMPLETE`; when it's
the backbone-only variant (or the artifact is `role:ckpt` via `SOURCE LAYOUT: diffusion_models`
with no text-encoder evidence at all), tag it `NEEDS PAIRING`. Surface it as a short badge next
to the role code in the table (`CKPT` becomes `CKPT · COMPLETE` or `CKPT · NEEDS PAIRING`) and
as a facet value under `ROLE` (a sub-row, the way sub-roles like `ipa`/`tok`/`mot` already
render as chips per `docs/design/ui-spec.md`) rather than a whole new rail section — it's a
qualifier on `ckpt`, not a fourth independent axis, and only `ckpt`-role artifacts ever carry it.

## Recommendation 3: ship the brand/family grouping from the navigation review, ordered after SOURCE LAYOUT

The two-tier `BASE ARCHITECTURE` redesign in `catalog-navigation-review.md` (brand header,
variant rows beneath it, reusing the existing bidirectional prefix match in
`catalog/query.rs`'s `base_filter`) stands as designed — nothing here changes it. What this
review adds is where it sits relative to `SOURCE LAYOUT`: they are orthogonal axes answering
different questions (*what architecture is this* vs. *how was it filed / what ComfyUI slot does
it target*), not competing designs, and the rail should present them as separate sections in
that order — `SOURCE LAYOUT` (placement, ComfyUI-legible, always present for loose sources)
before `BASE ARCHITECTURE` (content, inferred, sometimes absent). Placing origin ahead of
inferred architecture matches the priority ComfyUI itself gives them: placement decides whether
a node picker lists the file at all; architecture only matters once you've already got it into
the right loader.

One caution the taxonomy doc's brand table needs to inherit: `qwen_image_2512_fp8_e4m3fn.safetensors`
(a diffusion checkpoint, `SOURCE LAYOUT: diffusion_models`) and `Qwen/Qwen3-0.6B` (a chat model,
`SOURCE LAYOUT: hub repo`) share the brand name "Qwen" but are unrelated products — a text
generation LLM family and an unrelated text-to-image DiT family from the same publisher. A
brand-alias table keyed on the `base` root (as Recommendation A in the navigation review
proposes) already avoids conflating them, since their inferred `base` values don't share a root
(`qwen3` vs. whatever `qwen_image_2512_fp8_e4m3fn.safetensors`'s — currently absent — base would
resolve to) — but it's worth stating explicitly as a constraint on that table's construction:
brand grouping keys off `base`, never off a literal "Qwen" string match against `org` or `name`,
or this exact conflation is one keyword-search shortcut away.

## Recommendation 4: rail layout, concretely

Against `docs/design/ui-spec.md`'s current facet rail (`ROLE`, `BASE ARCHITECTURE`,
`FORMAT · PRECISION`, `NODE`, then `SAVED SHELVES`):

```
ROLE
  ckpt                         40
    COMPLETE                    6
    NEEDS PAIRING               34
  llm                          58
  lora                         37
  ...
SOURCE LAYOUT
  checkpoints                  10
  diffusion_models             36
  text_encoders                 17
  loras                         21
  hub repo                      156
  (unplaced)                    5
BASE ARCHITECTURE
  QWEN                          33
    qwen3 ...
  ...
FORMAT · PRECISION
NODE
```

`ROLE`'s `COMPLETE`/`NEEDS PAIRING` sub-rows render only under `ckpt` and only when the split is
meaningful (hide them entirely if every `ckpt` artifact in the current result set carries the
same tag, the way empty facet sections already collapse per `renderFacets` in `web/js/facets.js`).
`SOURCE LAYOUT`'s `(unplaced)` bucket is the honest home for anything with no recognizable
ComfyUI folder and no hub repo — better than silently omitting it, for the same reason the
navigation review argued for an explicit `UNKNOWN PUBLISHER` bucket rather than dropping orgless
artifacts from a publisher view.

## What this deliberately does not do

- It does not change any confidence threshold, tensor rule, or the `base`/`role` fields
  `compat:` and the inspector depend on. Every recommendation here is a new, additional,
  presentation-level fact layered next to what already exists — consistent with the navigation
  review's constraint that grouping must never touch the compat-relevant exact values.
- It does not propose fixing the `text_encoders/` role split, the HiDream misclassification, or
  the base-sampling starvation at the inference level — those are correctness bugs already
  reported in conversation, and Recommendations 1-2 above are specifically about designing the
  IA to stay legible *around* imperfect inference rather than waiting on it to be fixed. If and
  when those bugs are fixed, `SOURCE LAYOUT` and `COMPLETENESS` remain useful as independent,
  corroborating facets — they don't become redundant.

## Top 3, in priority order

1. **`SOURCE LAYOUT` facet** (Recommendation 1) — cheapest, uses data already stored (`path`),
   touches no inference policy, and resolves three separate confusions (triage-by-folder-hint,
   the `text_encoders` role split, and the HiDream case) at once by giving the user back the one
   signal they already provided and hoard currently throws away.
2. **`COMPLETE` / `NEEDS PAIRING` tag on `ckpt`** (Recommendation 2) — also cheap (the evidence
   already exists, it just needs to be promoted out of the inspector's evidence list), and
   answers the single most practical question a ComfyUI user asks when browsing checkpoints.
3. **Ship the brand/family rail redesign, ordered after `SOURCE LAYOUT`** (Recommendation 3) —
   already fully designed in the navigation review; this review's only addition is sequencing it
   correctly relative to the new facet and flagging the brand-table conflation risk before it's
   built.
