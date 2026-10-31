# Catalog improvements: the implementation plan

Historical decision brief. Its four waves landed, and later corrections replaced parts of
the rail design. See [current design status](README.md); do not rerun these waves or bumps.

Status: decided. This is the reconciliation of `docs/design/catalog-navigation-review.md`
(the facet rail review), `docs/design/comfyui-model-taxonomy.md` (the ComfyUI folder
research), `docs/design/catalog-ia-recommendations.md` (their synthesis) and the three
correctness bugs found against this workstation's real cache while writing them. Those three
documents propose; this one decides. Where they disagree with it, this document wins.

No application code was changed to produce it. It is not itself a wave plan: it is the brief a
wave plan is written from, and it is written so that writing those plans needs no further
design work. `.claude/skills/hoard-write-plan/SKILL.md` says what the plans themselves have to
carry.

## Wave status

| Wave | Name | Ships | `FORMAT_VERSION` | Crates touched |
| --- | --- | --- | --- | --- |
| 1 | `infer-pairing` | sample starvation, fused-image role, agreeing folder and name, COMPLETE/BACKBONE | bump | `hoard-core`, `hoard-index` |
| 2 | `source-layout` | the `SOURCE LAYOUT` facet | bump | `hoard-index`, `hoard-server`, `web` |
| 3 | `rail-legibility` | shelf counts, triage count, full labels, rail cap and filter | no | `hoard-index`, `hoard-server`, `web` |
| 4 | `base-brands` | the two-tier `BASE ARCHITECTURE` rail | no | `hoard-index`, `hoard-server`, `web` |

Each wave lands on `master` as exactly one squashed commit, in that order. Nothing in a later
wave is a prerequisite of an earlier one.

## Verdict on every proposal on the table

| # | Proposal | Source | Verdict |
| --- | --- | --- | --- |
| a | HiDream fused checkpoint reads `role: llm` | conversation | Wave 1 |
| b | `FOLDER_HINT` 0.35 sits below `ROLE_FLOOR` 0.5 | conversation | **rejected**, the floor stays |
| c | `select_samples` starves the base rules of keyword slots | conversation | Wave 1 |
| d | `role:unk` where the folder and the file name name the same component | conversation | Wave 1 |
| 2 | `COMPLETE` / `NEEDS PAIRING` on `ckpt` | ia-rec 2 | Wave 1, as a sub-role |
| 1 | `SOURCE LAYOUT` facet from `path` | ia-rec 1 | Wave 2 |
| - | shelves carry no result count | conversation | Wave 3 |
| E | triage count buried in `SAVED SHELVES` | nav review E | Wave 3 |
| C | truncated rail rows, no `title` | nav review C | Wave 3, the mechanical half only |
| D | silent 32-bucket cap, no type-to-filter | nav review D | Wave 3 |
| A | two-tier brand/variant `BASE ARCHITECTURE` rail | nav review A, ia-rec 3 | Wave 4 |
| B | `PUBLISHER` facet from `org` | nav review B | deferred |
| C' | hand-written human labels per base value | nav review C | deferred |
| 6 | Cards view shows no brand signal for adapters | nav review 6 | deferred |
| - | `brand:` / `family:` query token | nav review open question | deferred |
| - | fix the `text_encoders/` role split at the inference level | taxonomy doc | **rejected** |

Reasons for every deferral and both rejections are in "What is not being built", below.

## The calls that settle the design

These are the decisions the three review documents left open or got half right. A wave plan
must not relitigate them.

### The correctness bugs belong in this plan, ahead of the IA

Bugs (a) and (c) are Wave 1 rather than separate work, because the IA sits directly on their
output and cannot be evaluated over it as it stands. `base` is null for 100 of 233 artifacts
(43%) overall, and for 20 of the 40 artifacts correctly tagged `ckpt` under `checkpoints/` and
`diffusion_models/` (6 of 6 and 14 of 34) - and the sampling-starvation bug is a confirmed
mechanism for at least one of them and a plausible one for a meaningful share. Wave 4 redesigns
the facet built from exactly that field. Shipping a brand rail first would mean rebuilding the
`BASE ARCHITECTURE` section over an input that is empty for half of its most important role,
with no way to tell a grouping failure from a missing value. The same argument puts (a) here:
`checkpoints/hidream_o1_image_dev_fp8_scaled.safetensors` is the worked example the taxonomy
document uses, and leaving it wrong while shipping a facet whose stated purpose is to explain
it is not a defensible sequence.

Bug (b) is not deferred; it is rejected outright. Bug (d) is a different claim from (b) and is
built; the next section says why the two are not the same argument.

The shelf-count gap is Wave 3 rather than separate work because proposal E - a persistent
triage count - is the same code path: there is no count to put in a badge until
`GET /api/v1/shelves` computes one. Splitting them would be two waves over one function.

### A folder and a file name that agree are not the same claim as a folder alone

Rejecting (b) is not a reason to reject (d), and treating them as one argument was an error in
the review this plan reconciles. (b) asks a folder to classify on its own, which is weak:
`checkpoints/` holds files that are not checkpoints, which is exactly what
`checkpoints/anima/*` and `hunyuan_3d_v2.1.safetensors` demonstrate. (d) asks two independent
weak signals that agree to classify together, which is a different and much stronger claim, and
it was never evaluated on its own terms anywhere in the three source documents.

Three files on this cache make the case concretely. `vae/mage_flow_vae_bf16.safetensors`
carries only `{"format": "pt"}` in `__metadata__` and a bespoke tensor namespace
(`student.dconv_encoder`, `pipeline.blocks`, `pipeline.y_embedder`);
`vae/trellis_2_shape_vae_bf16.safetensors` and `vae/trellis_2_texture_vae_bf16.safetensors`
carry no `__metadata__` at all and a namespace of `shape_dec.blocks` / `struct_dec.blocks`. No
tensor rule recognises any of them, so Wave 1's metadata read does nothing for them - there is
no `model_type` to read - and Wave 2's `SOURCE LAYOUT` is presentation-only by decision, so it
would show `vae` beside them and still leave them `role:unk`. All three sit directly in `vae/`
and all three say `vae` in their own file name.

**The rule ships, in Wave 1, and it needs no new constant, no new threshold and no change to
`ROLE_FLOOR`.** The confidence arithmetic the codebase already has encodes exactly the policy
wanted here. `FOLDER_HINT` is 0.35, `FILENAME_HINT` is 0.4, `REINFORCEMENT` is 0.5 and
`ROLE_FLOOR` is 0.5, so with `infer::combine`:

- a folder hint alone is 0.35 and classifies nothing;
- a file-name hint alone is 0.4 and classifies nothing;
- the two agreeing are `0.4 + (1 - 0.4) * 0.35 * 0.5 = 0.505`, which clears the floor.

Agreement is therefore required without anything having to require it: the `Accumulator` scores
each candidate role separately, so a name and a folder that disagree - `vae_prompt_helper.safetensors`
sitting in `checkpoints/` - leave `vae` at 0.4 and `ckpt` at 0.35, both under the floor, and the
artifact stays unclassified. No special case, no conjunction rule, no second code path.

Two further properties make this safe rather than merely clever. Any tensor rule outranks it:
the weakest, `TENSOR_SHAPE`, is 0.88. And the default `triage_confidence` is 0.85, so a file
rescued at 0.505 is given a usable role for the rail, for `role:` and for `compat:` **and stays
in the triage queue** for a human to confirm. The rule cannot launder a guess into a settled
fact, which is the specific harm the `ROLE_FLOOR` policy exists to prevent.

The 0.005 margin is thin, and a later edit to any of those four constants would silently
un-rescue every file this fixes, so it is pinned by an assertion rather than left to arithmetic
nobody reads. Noted while checking it, and unchanged by this wave: `folder_hints` already emits
one hint per matching path component, so three nested folders naming the same role already
combine to about 0.558 and already classify today. That is pre-existing, not introduced here.

### `COMPLETE` / `NEEDS PAIRING` is a sub-role, not a new field and not a new facet dimension

`SubRole` already exists, is already stored on `Artifact`, is already indexed as the `sub_role`
keyword, is already narrowed by `role_filter` in `catalog/query.rs`, and already renders as a
chip per `docs/design/ui-spec.md`. `RoleRule` already carries a `sub_role` slot. Adding two
variants under `Role::Ckpt` is the whole mechanism; a new stored field, a new record, a sixth
facet dimension or a bespoke badge type would all be new machinery for a distinction the
existing one expresses exactly.

Names: `SubRole::Cmp` (`as_str` `cmp`, `label` `COMPLETE`) and `SubRole::Bb` (`as_str` `bb`,
`label` `NEEDS PAIRING`), both with `role() == Role::Ckpt`. Aliases must not collide with
`Role::Ckpt`'s existing aliases, which already include `unet` and `diffusion`: use
`complete`/`all-in-one` and `backbone`/`needs-pairing`, and nothing else.

### The pairing tag must not be decided by the confidence accumulator

This is the trap in the obvious implementation. `infer::combine` reinforces: each further
piece of evidence closes half the remaining gap, so two backbone rules at `TENSOR_PATTERN`
(0.90) and `TENSOR_SHAPE` (0.88) combine to roughly 0.988 and beat a single `COMPLETE` claim at
`TENSOR_STRONG` (0.95). A complete SD-family checkpoint fires both the complete rule and two or
three backbone rules at once - the backbone rules forbid adapters, not text encoders - so
routing the tag through `Accumulator<SubRole>` would tag most complete checkpoints
`NEEDS PAIRING`, which is worse than no tag.

So the tag is decided deterministically, after the role is settled, by one function in
`crates/hoard-index/src/infer/role.rs`:

```rust
fn pairing_sub_role(tensors: &TensorSummary) -> Option<SubRole>
```

  It runs only when the winning role is `Role::Ckpt`. It answers `Cmp` when the header carries
  a diffusion denoiser together with a text encoder (`DIFFUSION_KEYS` with `TEXT_ENCODER_KEYS`)
  or the latent-diffusion pair (`first_stage_model` segment with a `model`/`model_ema`
  segment); `Bb` when a denoiser-shaped body matched but neither of those did; `None` when
  neither applies. Its answer replaces whatever `sub_roles.best()` produced for a `ckpt`
  artifact, which today is always `None`, so nothing existing changes meaning.

  The rule "a text encoder body holding the whole file" gets no tag. A standalone T5 or CLIP is
  not a diffusion backbone, and calling it `NEEDS PAIRING` would make the label mean two
  different things. `SOURCE LAYOUT: text_encoders` is what answers that case.

### The safetensors architecture read is positive-only

`architecture_rule` reads GGUF's `general.architecture` and, when the token is not a known
diffusion architecture, concludes `Role::Llm` at `METADATA_STRONG`. Extending that rule
verbatim to safetensors `__metadata__.model_type` would make the HiDream case worse, not
better: `hidream_o1_image` is not in `DIFFUSION_ARCHITECTURES`, so the extended rule would
assert `llm` at 0.9 and cement the misclassification the change is meant to fix.

The safetensors read therefore claims `Role::Ckpt` and never `Role::Llm`. An unrecognised
`model_type` in a safetensors header records no role evidence at all. `DIFFUSION_ARCHITECTURES`
gains the families this cache actually holds (`hidream`, `qwen_image`, `hunyuan`, `chroma`) and
stays a prefix table ordered like the rest of `rules.rs`; it is not a place to speculate about
architectures nobody here has on disk.

### `SOURCE LAYOUT` is derived at index time and stored nowhere

No native_db record changes shape in Wave 2. The value is a pure function of three things every
writer and every reader already has: the source's `SourceKind`, the source's root path, and the
entry's absolute path (`Repository::root_path` joined with `Entry::rel_path`, or
`Placement::resolve`). The same function serves `Fields::document` at index time and
`assemble_row` at read time, so the facet and the inspector can never disagree.

It is an entry-level value, like `node` and `state`: the same blob linked into `checkpoints/`
and present in the hub cache is one artifact sitting in two layouts. A `layout:` token must
therefore be added to the entry-level arm of `filters_on_an_entry` in `catalog/query.rs`, or a
query naming it will answer from primary documents and silently under-report.

The match rule is the **outermost** path component beneath the source root that names a
registered folder, which is what makes `checkpoints/anima/vae/x.safetensors` read
`checkpoints` rather than `vae`, and mirrors ComfyUI's own mechanism: only top-level registered
folders are categories, and `recursive_search` walks into everything below one.

Sources of kind `HfCache` read `hub repo` and sources of kind `Git` read `git clone` without
consulting the folder table at all. This is not cosmetic. A diffusers snapshot genuinely
contains `unet/`, `vae/` and `text_encoder/` subdirectories, so running the folder table over
an hf-cache path would file most of the HuggingFace cache under ComfyUI category names that
mean nothing there. A file under a `Loose` source that matches no registered folder reads
`(unplaced)`.

### Both Wave 1 and Wave 2 bump `catalog::FORMAT_VERSION`

Per the "Compatibility" section of `CLAUDE.md` and the rule already recorded in
`docs/native-catalog.md`: a sniffing or inference rule that changes is a `FORMAT_VERSION` bump
and nothing else, because an hf-cache blob's inode, mtime and size do not move when the rule
that reads it does, so a rescan would settle every stored revision on its stamps and never
re-infer anything. Wave 1 changes which tensor samples a header keeps and what role and
sub-role a file gets: bump. Wave 2 adds a keyword column to the Tantivy schema, and a published
`search-v1-*` directory written against the old schema cannot be read through the new `Fields`:
bump.

A cheaper Wave 2 is technically available - everything the document builder needs is already in
the native_db records, so a search rebuild alone would suffice - and it is rejected. Triggering
it would need a second version stamp on the search directory and a branch on it, which is the
version ladder the Compatibility section forbids. One bump, one rebuild from disk, no stamp, no
branch.

Waves 3 and 4 change no stored value and no schema, and must not bump.

### A brand row is a union of its members, never a prefix

The caution in `catalog-ia-recommendations.md` - key the brand table off the inferred `base`
root, never off a literal name match against `org` or `name` - is right but insufficient, and
the shortfall matters enough to state as a constraint on Wave 4.

`Base::new` strips underscores, so `qwen_image_2512` normalises to `qwenimage2512`, which
*does* share the literal root `qwen` with `qwen3`. The existing bidirectional prefix rule in
`base_filter` would therefore sweep the Qwen-Image diffusion family into a `QWEN` brand row
built from the prefix `qwen` - exactly the conflation the recommendation warns about, arriving
by the route it declared safe.

So a brand row carries an explicit union of the exact base values the table assigns to that
brand and that the current result set actually holds: `base:qwen3,qwen35,qwen35moe,qwen3vl`,
not `base:qwen`. Its count is that union query's own count, not the sum of its children's,
because a child's count is already a prefix count (`qwen3`'s bucket count includes every
`qwen35` artifact) and summing them double-counts. The brand table maps exact base roots to a
brand, and a base value the table does not name stays an ungrouped top-level row.

## Wave 1: `infer-pairing`

**Goal.** A checkpoint's base architecture is inferred from a sample set that reaches its
cross-attention projections; a fused image model is a checkpoint; a file whose folder and whose
name name the same component is that component; and every `ckpt` artifact says whether it is
usable alone.

### 1.1 Per-keyword floor in the tensor sample draw

`crates/hoard-index/src/sniff/mod.rs`, `select_samples` and `KEYWORD_BUDGET`.

`select_samples` reserves 32 of 64 slots for keyword-flagged tensors and fills them greedily in
name-sorted order across 21 competing `SAMPLE_KEYWORDS` with no per-keyword floor. For
`checkpoints/pony/cyberrealisticPony_v160.safetensors` (2,515 tensors) the VAE's
`first_stage_model.*` tensors exhaust the whole keyword budget before the walk reaches a
`to_k`, so `cross_attention_rule` in `infer/base.rs` never sees the
`attn2.to_k.weight` shaped `[640, 2048]` that names the file SDXL, and the artifact stores no
base.

The fix: one pass over the sorted tensors recording, per keyword, the index of its first match;
those indices are admitted first; the remainder of `KEYWORD_BUDGET` is then filled greedily as
today, and the even-spread pass is unchanged. Cost stays one pass and a 21-entry array, so
nothing about the sniff's scale behaviour moves.

No record changes shape. `ArtifactDetail`'s stored samples change content, which is what the
version bump is for.

Test `every_sample_keyword_keeps_a_slot_of_its_own`: a synthetic header of several hundred
`first_stage_model.*` tensors sorting ahead of one
`model.diffusion_model.input_blocks.4.1.transformer_blocks.0.attn2.to_k.weight` shaped
`[640, 2048]` asserts that the `to_k` sample survives `summarize_tensors`, and that
`infer_base` over that summary answers `Base::new("sdxl")`. The existing
`keyword_matches_cannot_starve_the_structural_spread` and
`samples_are_capped_and_keep_the_first_largest_and_keyword_keys` keep their assertions.

### 1.2 A safetensors metadata architecture names a checkpoint

`crates/hoard-index/src/infer/role.rs`, `architecture_rule`; `crates/hoard-index/src/infer/rules.rs`,
`DIFFUSION_ARCHITECTURES`.

`architecture_rule` reads only GGUF's `general.architecture`, so
`hidream_o1_image_dev_fp8_scaled.safetensors` - whose `__metadata__` names
`model_type: hidream_o1_image` - is classified by its fused `language_model.*` component alone
and takes `role: llm` uncontested. The rule gains a second, positive-only read of
`model_type` from the same `TensorSummary::metadata` map (safetensors `__metadata__` is already
captured verbatim there by `sniff/safetensors.rs`), claiming `Role::Ckpt` at `METADATA_STRONG`
when the token prefixes a known diffusion architecture and claiming nothing otherwise.

Test `a_fused_image_model_is_a_checkpoint_not_a_language_model`: a summary carrying
`language_model.model.layers.0.self_attn.q_proj.weight` and `lm_head.weight` plus metadata
`model_type = hidream_o1_image` asserts `role == Role::Ckpt`.
Test `an_unrecognised_metadata_model_type_names_no_role`: the same tensors with
`model_type = someones_merge` assert the verdict carries no `EvidenceSource::Metadata` role
evidence, and the role is still `Role::Llm` from the tensor rule.

### 1.3 A file name that names a component, agreeing with its folder

`crates/hoard-index/src/infer/rules.rs` (a new `FILENAME_ROLE_HINTS` table beside
`FOLDER_ROLE_HINTS`) and `crates/hoard-index/src/infer/role.rs` (`name_rules`, which already
emits filename evidence for tokenizer files). No new confidence constant: the claim is pushed
at the existing `rules::FILENAME_HINT` (0.4) with `EvidenceSource::Filename`, which today only
`infer/trigger.rs` uses.

```rust
pub const FILENAME_ROLE_HINTS: &[(&str, Role, Option<SubRole>)]
```

  The table holds unambiguous component nouns only, with the same tuple shape as
  `FOLDER_ROLE_HINTS` so the two stay legible side by side: `vae` and `autoencoder` name
  `Role::Vae`; `lora`, `lycoris`, `locon` and `loha` name `Role::Lora`; `ipadapter` names
  `Role::Lora` with `SubRole::Ipa`; `controlnet` names `Role::Ctrl`; `upscale`, `upscaler`,
  `esrgan` and `swinir` name `Role::Upsc`; `unet` names `Role::Ckpt`.

  A token is in the table only when it names exactly one component type and appears in file
  names for no other reason. `model`, `base`, `checkpoint`, `text`, `encoder` and `embedding`
  are excluded as generic. `clip` is excluded because it appears inside the names of artifacts
  that merely contain a CLIP encoder rather than being one. `diffusion` is excluded for the
  sharpest reason: `diffusion_pytorch_model.safetensors` is the universal diffusers component
  file name and appears identically under `unet/`, `vae/` and `text_encoder/`, so the token
  names no component at all.

  Matching is on whole tokens of the file name with its extension removed, split on every
  non-alphanumeric character, never on substrings - otherwise `lora` matches `explorations`.
  The rule pushes one claim per distinct role the tokens name and nothing else; it does not
  consult the folder, because `folder_hints` already pushes the folder's claim into the same
  accumulator and agreement is what `combine` then does with the two.

No record changes shape. The wave's bump covers it, because a rescan cannot re-infer a file
whose inode, mtime and size have not moved.

Test `a_file_name_and_a_folder_that_agree_clear_the_role_floor`: a summary of bespoke keys
(`shape_dec.blocks.0.weight`, `struct_dec.blocks.0.weight`) that no rule matches, at
`/models/vae/trellis_2_shape_vae_bf16.safetensors`, asserts `role == Role::Vae`, asserts
`confidence >= ROLE_FLOOR`, and asserts the verdict carries both an `EvidenceSource::FolderHint`
and an `EvidenceSource::Filename` entry. This is the pin on the 0.005 margin; it fails loudly
if any of the four constants moves.

Test `a_file_name_hint_alone_stays_unclassified`: the same tensors at
`/data/weights/trellis_2_shape_vae_bf16.safetensors`, under no registered folder, assert
`Role::Unk` and `confidence < ROLE_FLOOR`. Its sibling
`a_folder_hint_alone_stays_unclassified` already exists and keeps its assertions unchanged -
its fixture, `mystery.safetensors`, carries no component noun, which is why it still holds.

Test `a_file_name_that_disagrees_with_its_folder_classifies_nothing`: the same tensors at
`/models/checkpoints/vae_prompt_helper.safetensors` assert `Role::Unk`, because the two weak
claims land on different roles and neither reaches the floor alone.

Test `a_rescued_role_still_needs_triage`: the rescued verdict's confidence is below the default
`triage_confidence` of 0.85, so `Artifact::needs_triage(0.85)` answers true.

Test `a_novel_architecture_without_a_component_noun_stays_unclassified`: bespoke keys at
`/models/checkpoints/anima/animaOfficial_preview2.safetensors` and at
`/models/checkpoints/hunyuan_3d_v2.1.safetensors` assert `Role::Unk`. These are the taxonomy
document's own triage cases, and they must stay triaged: the rule is narrow by construction,
and this test is what says so.

### 1.4 `COMPLETE` and `NEEDS PAIRING`

`crates/hoard-core/src/role.rs` (`SubRole::Cmp`, `SubRole::Bb`, their `as_str`, `role`, `label`
and `aliases`); `crates/hoard-index/src/infer/role.rs` (`pairing_sub_role`, applied in
`infer_role`). `Role::Ckpt::aliases` is not touched.

Stored shape: `Artifact::sub_role` is already `Option<SubRole>`; the enum gains two variants, so
the set of values a scan can write changes and the bump applies.

Test `a_checkpoint_with_a_text_encoder_is_complete`: `model.diffusion_model.*` with
`cond_stage_model.*` asserts `sub_role == Some(SubRole::Cmp)`.
Test `a_backbone_checkpoint_needs_pairing`: `double_blocks.*` / `single_blocks.*` alone asserts
`Some(SubRole::Bb)`.
Test `a_complete_checkpoint_is_not_downgraded_by_its_backbone_rules`: a header that fires the
complete rule and two backbone rules at once asserts `Some(SubRole::Cmp)` - this is the
accumulator hazard, pinned.
Test `a_standalone_text_encoder_carries_no_pairing_tag`: `text_model.encoder.*` alone asserts
`sub_role == None`.

### 1.5 The bump and the docs

`crates/hoard-index/src/catalog/mod.rs`: `FORMAT_VERSION` 8 to 9, and nothing else - no
converter, no ladder, no snapshot. `docs/api/hoard-api.md` gains `cmp` and `bb` to the
`sub_role` vocabulary line. `docs/design/ui-spec.md` records the `CKPT - COMPLETE` /
`CKPT - NEEDS PAIRING` chip beside the existing `ipa`/`tok`/`mot` chips.

## Wave 2: `source-layout`

**Goal.** A rail section reports the folder a file was found in, so the placement decision the
user already made is visible whatever content inference concluded.

### 2.1 The layout table and its function

New module `crates/hoard-index/src/layout.rs`, listed in `docs/architecture.md` under the
`hoard-index` tree as the rules say. It holds the registered-folder table - folder spelling to
canonical layout value, with ComfyUI's own aliases folded (`unet` to `diffusion_models`, `clip`
to `text_encoders`, `t2i_adapter` to `controlnet`, `vae_approx` to `vae`) and the A1111
spellings mapped onto the same values (`stable-diffusion` to `checkpoints`, `lora` to `loras`,
`esrgan` to `upscale_models`) - and one function over `SourceKind`, the source root and the
absolute path.

It is deliberately not `FOLDER_ROLE_HINTS`. That table maps a folder to a `Role` and folds
`unet` and `diffusion_models` and `text_encoders` all onto `Role::Ckpt`, which is precisely the
distinction this facet exists to restore. The two tables stay separate and the new module's
doc comment says why.

Test `a_comfyui_folder_names_the_source_layout`: `unet/` and `diffusion_models/` both answer
`diffusion_models`; `clip/` and `text_encoders/` both answer `text_encoders`.
Test `the_outermost_registered_folder_wins`: `checkpoints/anima/vae/x.safetensors` answers
`checkpoints`.
Test `an_hf_cache_entry_is_a_hub_repo_whatever_its_subfolders_are_called`: an `HfCache` source
whose entry sits at `snapshots/<sha>/unet/diffusion_pytorch_model.safetensors` answers
`hub repo`.
Test `a_file_under_no_registered_folder_is_unplaced`.

### 2.2 The sixth facet dimension

`crates/hoard-index/src/search/facets.rs` (`FACET_DIMENSIONS`, `Facets` and both `dimensions`
accessors), `crates/hoard-index/src/search/token.rs` and `grammar.rs` (a `layout:` token),
`crates/hoard-index/src/catalog/search/schema.rs` (`LAYOUT_FIELD`, `KEYWORD_FIELDS`,
`COLUMN_KEYWORDS`, `Fields::document`), `crates/hoard-index/src/catalog/search/tally.rs`
(`FACET_COLUMNS`, `LABEL_COLUMN`), `crates/hoard-index/src/catalog/search/answer.rs` (the
`facets` loop's destructuring), `crates/hoard-index/src/catalog/query.rs` (`clause` and the
entry-level arm of `filters_on_an_entry`).

The rail order is `ROLE`, `SOURCE LAYOUT`, `BASE ARCHITECTURE`, `FORMAT - PRECISION`, `NODE`:
placement before inferred architecture, for the reason `catalog-ia-recommendations.md` gives -
placement decides whether a loader lists the file at all, architecture only matters once it is
in the right loader.

`LABEL_COLUMN` in `tally.rs` is derived from `FACET_COLUMNS.len()` and moves with it; a plan
must name it explicitly, because it is the one place the sixth dimension silently shifts an
index.

Test `the_layout_facet_counts_the_folder_an_entry_sits_in`: a seeded catalog with entries under
two ComfyUI folders and one hub repo asserts the three buckets and their counts.
Test `a_layout_token_filters_on_an_entry`: `needs_collapse` answers true for `layout:loras`,
because two entries of one artifact can sit in two layouts.
Test `the_rail_names_six_dimensions_in_order` replaces
`the_rail_names_five_dimensions_in_order`.

### 2.3 The row, the API and the rail

`SearchRow` in `crates/hoard-index/src/row.rs` gains `layout: Option<String>`, assembled in
`catalog/search/answer.rs::assemble_row` by the same function the document builder calls.
`crates/hoard-server/src/api/search.rs` carries the new dimension through `facets()` with no
special casing. `web/js/facets.js` gains the section to `SECTIONS`. The inspector shows the
layout as a fact (`crates/hoard-server/src/api/artifacts.rs`).

`docs/api/hoard-api.md` documents `facets.layout`, the `layout:` key and the row field;
`docs/design/ui-spec.md` records the new rail section and its position.

Release asset test after this wave: `cargo test --release -p hoard-server --lib assets::`.

### 2.4 The bump

`FORMAT_VERSION` 9 to 10.

## Wave 3: `rail-legibility`

**Goal.** Nothing in the rail is a number the user has to click to discover, a string the rail
clipped without saying so, or a bucket the rail dropped without saying so.

No stored record and no schema changes. No bump.

### 3.1 Shelves report their result count

`crates/hoard-index/src/catalog/userdata.rs` and `crates/hoard-index/src/catalog/search/answer.rs`:
one catalog call that counts every shelf's query against one searcher, rather than one query per
shelf per caller. `crates/hoard-server/src/api/shelves.rs`: `ShelfView` gains
`count: Option<u64>`, null for a shelf whose query does not compile - an unparseable saved query
is a fact about that shelf, never a reason for the route to fail.

`web/js/shelves.js` renders the count in both places that list shelves: the rail row and the
`SHELVES` tab, which today shows `NAME`, `QUERY`, `KIND` and nothing else.

Test `a_shelf_reports_how_many_artifacts_its_query_answers`: over a seeded catalog, the
`triage queue` shelf's count equals the catalog's triage artifact count, and a shelf whose query
names a shelf that does not exist reports `None` while its siblings still report numbers.

### 3.2 The triage count is persistent

Proposal E, resolved by 3.1 plus a badge: the `SAVED SHELVES` title carries the `triage queue`
shelf's live count, and that shelf sorts first in the rail list. 25 of 235 artifacts (11%) are
in triage on this node and today nothing says so until you click.

The `ROLE` rail already carries a `unk` row with its own count, and `docs/design/ui-spec.md`
already specifies `-` for a zero count, so no second mechanism is added for the role side.

### 3.3 Full labels, no silent truncation

`web/js/facets.js`, `facetRow`: every facet row gets a `title` attribute carrying the full
label and, where they differ, the raw value - `qwen3_tts_tokeniz...` becomes recoverable on
hover. This is the mechanical half of proposal C. The editorial half is deferred; see below.

### 3.4 Rail scale affordances

The 32-bucket base cap is silent today, and the unfiltered rail on this node already sits at 27
of 32. `Facets` gains `base_total: u64` - what `count_buckets` counted before
`base_candidates` and `truncate(BASE_RAIL)` cut it - carried through
`crates/hoard-server/src/api/search.rs` and rendered by `web/js/facets.js` as
`27 of 41 - showing the most common` whenever it exceeds the rows shown.

A type-to-filter input appears above any rail section over 15 rows. It is a substring match
over rendered rows and nothing else: it issues no query, parses no grammar and computes nothing
a unit test could hold, which is what keeps it inside the "no logic in JS" rule.

Test `the_base_rail_reports_how_many_bases_it_could_not_show`: a seeded catalog with more than
32 distinct bases asserts `base_total` exceeds `base.len()`, and that `base.len()` is
`BASE_RAIL`.

`docs/api/hoard-api.md` documents `facets.base_total` and the shelf `count`.

## Wave 4: `base-brands`

**Goal.** One model brand occupies one rail entry with its variants beneath it, without
touching the exact `base` value `compat:` matches on.

No stored record and no schema changes. No bump.

### 4.1 The brand table

New module `crates/hoard-index/src/search/brands.rs`, listed in `docs/architecture.md`. It maps
exact `base` roots to a brand key and a display label, in the spirit of `BASE_ALIASES` but for
presentation only: `qwen3 -> ("qwen", "QWEN")`, `qwen35 -> ("qwen", "QWEN")`,
`qwen3vl -> ("qwen", "QWEN")`, `gemma3`/`gemma4 -> ("gemma", "GEMMA")`,
`lfm2`/`lfm2moe -> ("lfm", "LFM")`, `nemotronh`/`nemotronhmoe -> ("nemotron", "NVIDIA NEMOTRON")`.
It is keyed on exact roots, never on a prefix and never on `org` or `name`.

The table does not name `sd15`, `sd21`, `sd3` or `sdxl`: `BASE_ALIASES` already keeps those four
as separate families on purpose, because an SD1.5 LoRA does not load on an SDXL checkpoint, and
a `STABLE DIFFUSION` brand header collapsing them would put a wrong answer above four right
ones. A brand with one member in the current result set renders as today's single row; the
hierarchy appears only where it resolves something.

Test `a_brand_row_unions_its_members_and_never_prefixes_them`: bases `qwen3`, `qwen35moe` and
`qwenimage2512` present, the `QWEN` family holds the first two, its query is
`base:qwen3,qwen35moe`, and `qwenimage2512` is not in it despite sharing the literal prefix.
Test `every_brand_root_is_a_normalised_base`: a well-formedness test over the table, in the
shape of `base_aliases_are_ordered_from_specific_to_general`.

### 4.2 The grouped rail

`crates/hoard-index/src/search/facets.rs`: `Facets` gains
`base_families: Vec<FacetBucket>`, and `FacetBucket` gains `group: Option<String>` naming the
brand a base row belongs to. Both are marked by the existing `mark_active`, which already
prefix-matches base values through `Base::matches`.

`crates/hoard-index/src/catalog/search/answer.rs::facets`: after the existing per-bucket prefix
counting, the families the table names over the surviving buckets are counted, one union query
each, bounded by the table's size and therefore by a constant, the way `BASE_RAIL` bounds the
bucket counts.

`crates/hoard-server/src/api/search.rs`: families carry `token` and `query` set to the union,
not to `base:<brand>`. `web/js/facets.js`: a family renders as a header row with its count and
its members indented beneath it; a base row with no family renders exactly as today.

Test `a_family_counts_each_artifact_once`: an artifact based `qwen35` appears in both the
`qwen3` and `qwen35` prefix buckets, and the `QWEN` family count is the union's count, not the
sum of its rows.

`docs/api/hoard-api.md` documents `facets.base_families` and the `group` field;
`docs/design/ui-spec.md` records the two-tier rail.

Release asset test after this wave.

## What is not being built

**Rejected: lowering `FOLDER_HINT` or raising a folder hint above `ROLE_FLOOR`.** The floor
exists so a folder name can never overwrite a tensor verdict, and `SOURCE LAYOUT` gives the user
the folder without spending that guarantee. `a_folder_hint_alone_stays_unclassified` in
`infer/role.rs` and the floor assertion in `infer/mod.rs` are pinned tests for every wave here.
Task 1.3 is not a weakening of this: it adds a second, independent weak signal and leaves every
constant where it is, so a folder on its own still classifies nothing and both pinned tests keep
their assertions verbatim.

**Rejected: collapsing the `text_encoders/` role split at the inference level.** The split
tracks a real architectural fact - T5 and UMT5 are encoder-decoder and do not match the causal-LM
rule; Gemma, Mistral and Qwen decoder-only stacks do - and `role:llm` should keep meaning "the
tensors are a language model". `SOURCE LAYOUT: text_encoders` answers the different question the
user is actually asking, which is what ComfyUI does with the file.

**Deferred: the `PUBLISHER` facet (proposal B).** 205 of 233 artifacts (88%) carry no `org`, so
the facet would be one enormous `UNKNOWN PUBLISHER` bucket and a short tail; it earns a rail
section when a source population makes it discriminating.

**Deferred: hand-written human labels per base value (the editorial half of proposal C).** No
rule generates `Qwen3 TTS Tokenizer (12 kHz)` from `qwen3_tts_tokenizer_12hz`; a table of
prose strings is editorial work with no source of truth, and the `title` attribute in Wave 3
removes the actual harm.

**Deferred: a brand signal in Cards view (finding 6).** The fix is probably to show a LoRA's
declared or inferred compat target rather than its `base`, which is a different design question
than the rail's.

**Deferred: a `brand:` or `family:` query token.** Wave 4's union queries need no new grammar;
adding a token is a query-grammar decision against `docs/api/hoard-api.md` and can follow once
the rail has proved the grouping.

## Why this order

- Wave 1 before Wave 4: the brand rail regroups `base`, which is null for half the checkpoints
  under `checkpoints/` and `diffusion_models/` until the sampling fix lands. Regrouping a field
  before fixing it makes the result unevaluable.
- Wave 1 before Wave 2: not a technical dependency. `SOURCE LAYOUT` reads `path` and touches no
  inference policy, so it would work today. It is second so that the artifacts it is meant to
  explain - the HiDream file, the `checkpoints/anima/` triage cases - are already at their final
  role when the facet is judged, and so the two `FORMAT_VERSION` bumps land back to back rather
  than straddling the UI work.
- Wave 3 before Wave 4: the two-tier rail makes `BASE ARCHITECTURE` taller, and the cap
  indicator and the type-to-filter are what keep a taller rail usable. Shipping the redesign
  onto a rail that still truncates silently at 32 would trade one legibility problem for
  another.
- Waves 3 and 4 last: neither changes a stored value, so both can be revised on the evidence of
  the first two without another rebuild.

## What a wave plan still has to pin

Each wave plan is written from this document plus a fresh read of the code, and must carry, per
`.claude/skills/hoard-write-plan/SKILL.md`: the gate commit, the `Verified facts` block with
counts for every symbol named here, the pinned tests
(`a_folder_hint_alone_stays_unclassified`, `a_file_name_and_a_folder_that_agree_clear_the_role_floor`
from Task 1.3, `keyword_matches_cannot_starve_the_structural_spread`,
`a_base_filter_does_not_collect_architecture_agnostic_artifacts`,
`every_seeded_shelf_parses_and_compiles`), and the `DerivedShift` / `log_changes` obligations
from `.claude/skills/hoard-catalog/SKILL.md` for any wave that writes an entry - none of these
four do, which is itself worth stating in each plan's scope fence.

Fixtures stay synthetic headers generated in the test. The real cache is where a wave is
accepted, not where it is tested.
