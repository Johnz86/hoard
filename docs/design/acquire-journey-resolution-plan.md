# The opened artifact, partial downloads, the `any` row and base identification: the resolution plan

Historical decision brief. The waves and later follow-ups have landed. The “still open”
notes below reflect their original review dates; see [current design status](README.md).

Status: decided; no owner's call is open. Revised the same day after the owner ruled that Pony and
Illustrious are valid bases and a Civitai lookup by hash was measured (F9, D7). This document follows the read-only journey review of
2026-09-17 and the owner's verdict on it. It continues `catalog-ux-resolution-plan.md`, whose
fixes that review found holding, and it does not reopen anything that document decided.

It is not a wave plan. It is the brief wave plans are written from.
`.claude/skills/hoard-write-plan/SKILL.md` says what the plans themselves must carry.

No application code was changed to produce it. Every claim was read out of the tree at
`b8c1cc2` (catalog `FORMAT_VERSION` 12), checked against the owner's running server
(`target/debug/hoard serve`, 232 artifacts, 1.6 TB) through `/api/v1` and the Hub face, or read
from the real file headers on this workstation. Review findings 1 and 2 (free text and
`--revision`) are fixed on branch `wave/find-and-pull`, verified, and waiting to land; they are
not repeated here.

## What the owner ruled

| Finding | Verdict | Where it goes |
| --- | --- | --- |
| 3. Repo page describes the largest file, PULL is the whole folder | The page is a folder view by design and right for a model repository; wrong for a ComfyUI category folder; wants refinement, maybe download groups | D1, D2 |
| 4. `openai/gpt-oss-120b`, one shard of fourteen, reads as a whole local model | A broken download; probably should be ignored | D3 |
| 5. `any` lights up for every `base:` query | A bug | D4 |
| 6. Base identification | Serious gaps, needs investigation | D5 to D8 |
| Smaller: `(2 files)` suffix, latent upscalers, `int8` for mxfp4, blob path | not ruled on | D9 |

## Findings

### The opened artifact

**F1. The repository page never learns which artifact was opened.** `web/js/app.js:395` puts
`aid` in the URL and `:416` and `detail.js:80` use it for `+ LOADOUT` only. The server takes no
artifact: `api/repos.rs:490` (`pick_primary`) describes the largest weights file, `:306` takes
the header path from it, and `:332` calls `pull_commands(.., None, path)`. Measured: opening
`Z-Detail-Slider.safetensors` (21 MB) shows DERIVED FACTS for
`ltx-2.3-22b-distilled-lora-384.safetensors` (7.6 GB, rank 256) and copies
`hf download comfy/loras`, which is 38 files and 30.9 GB.

**F2. There are two kinds of loose repository and the catalog can tell them apart.**
`discover/loose.rs:28` recognizes a tool's models directory by a quorum of folder names and
then makes one repository per category folder, named `<source>/<folder>`; `locate` (`:407`)
gives a folder a real org only when the path starts `<org>/<name>/`. So `comfy/loras` is a bin
of 37 unrelated files whose org is the source's own name, while
`lmstudio-community/Qwen3-VL-8B-Instruct-GGUF` is one model and its projector under a
publisher's org. The `layout` field does not say this: `comfy/background_removal`,
`comfy/audio_encoders` and four more category folders read `(unplaced)`
(`layout.rs`, `LAYOUT_FOLDERS`). The reliable test is `repo.org() == the repository's source
name` on a loose source.

**F3. The page opens on `main` even when the artifact lives elsewhere.** For Ministral's shard
set (revision `613bd8d`) FILES lists the 5 files of `main`, none of them the artifact's, and
SWITCH offers only `refs/heads/main`. `GET .../files?revision=613bd8d` answers the 17 files, so
the data is reachable and the page does not ask for it.

**F4. Download groups already exist, and their script has the two bugs the pull command had.**
A loadout is a checkpoint with its adapters and weights (`L` adds the cursor row,
`POST /api/v1/loadouts/{id}/validate` checks compat, `apply` pulls and links).
`GET /api/v1/loadouts/{id}/export?format=hf-script` is the group download. But
`api/loadouts.rs:803` (`hf_script`) writes `hf download <repo> <pin.path>` itself: one shard of
a set, no `--revision`, no `--repo-type`. `catalog-ux-resolution-plan.md` D4 said it would call
`pull_commands`; it never did. And `web/js/loadout.js` has no control that reaches the export,
so the feature is API-only.

### Partial downloads

**F5. One shard of N is a whole model, two of N is a partial one.** `shard.rs:27`
`MIN_MEMBERS = 2`; `declared_set` (`:174`) and `named_sets` (`:227`) both return nothing for a
single present member, so the file falls through as a standalone artifact, is named as the
repository (`naming.single`), and never reaches the `incomplete` flag D9 of the previous plan
added. On disk: `models--openai--gpt-oss-120b` holds `model-00007-of-00014.safetensors` and an
index naming 14, with `.incomplete` partials and `.lock` files of 10 January beside it. The
catalog row reads `openai/gpt-oss-120b`, `llm`, 4.1 GB, `LOCAL`.

### The rail

**F6. `any` is a wildcard in the wrong place.** `search/facets.rs:117` (`selects`) decides a
bucket's `active` flag with `Base::matches`, and `hoard-core/src/role.rs:330` makes `any` match
everything, which is a compat rule. Measured: `q=base:flux` returns the `any` bucket
`active:true`; `q=base:any` returns all 17 buckets active over 45 rows.

### Base identification

**F7. The rules and the sampler are two hand-kept lists, and they disagree.** Every base and
role rule matches against `TensorSummary`: at most 32 leading key segments and 64 sampled
tensor names (`hoard-core/src/artifact.rs:268`, `has_fragment`). `sniff/mod.rs:30`
(`SAMPLE_KEYWORDS`) decides what earns a sample. `infer/rules.rs` names 92 distinct key
fragments; 67 of them are covered by no sampler keyword and match only if the even stride
happens to land on one. `label_emb`, `add_embedding`, `input_blocks`, `conditioner.embedders`
are among the 67. On a 2,515-tensor SDXL file neither SDXL rule can fire: the `to_k` slot goes
to `attn1.to_k` (640 by 640, replayed over the real header of `cyberrealisticPony_v160`), and
`label_emb` is 4 tensors of 2,515. The unit tests pass because each fixture holds only the key
its rule wants.

**F8. An exact-key family table closes most of the gap, and ComfyUI already maintains one.**
`/home/johnz86/GIT/ComfyUI/comfy/model_detection.py` names a family by one or two exact tensor
names after stripping `model.diffusion_model.` (for example `txtfusion.projector.weight` for
Krea 2, `video_patch_proj.weight` with `audio_patch_proj.weight` for MiniMax H3,
`layers.0.mlp.linear_fc2.weight` for Ernie, `label_emb.0.0.weight` for SDXL). Applied to the
real headers of all 55 safetensors checkpoints here:

| | Checkpoints with a family |
| --- | --- |
| hoard today | 23 of 55 |
| the exact-key table | 44 of 55 |
| newly named | 25 |
| disagreements with hoard's existing answers | 0 |

The 25: anima 3, sdxl 3, ernie 2, hidream-o1 1, ideogram4 2, ltx 4, mage-flow 3, minimax-h3 2,
qwen-image 2, trellis2 3. Mage-flow and qwen-image share `txt_norm.weight` and are told apart
by its width, read from the headers: 2560 against 3584. The 11 the table is silent on are text encoders filed as `ckpt`, the
five raw TRELLIS flow parts, the diffusers unet and one Flux file hoard already names.

**F9. The three disputed checkpoints are SDXL by architecture and Illustrious or Pony by base,
and only a lookup can say which.** Read from the headers, all three carry cross-attention width
2048, `label_emb`, a second text embedder and 2,514 to 2,516 tensors: one architecture, and no
header can tell the lineages apart. None carries metadata. Asked by sha256
(`GET https://civitai.com/api/v1/model-versions/by-hash/<sha256>`), Civitai answers:

| File | Civitai model and version | `baseModel` |
| --- | --- | --- |
| `oneObsession_v24.safetensors` | One obsession, v24 | Illustrious |
| `loxsSoulsMixPerfectDoll_v30.safetensors` | Lox's Souls Mix: Perfect Doll, V3.0 | Illustrious |
| `cyberrealisticPony_v160.safetensors` | CyberRealistic Pony, v16.0 | Pony |

One Obsession (model 1318945) has 25 versions: one is Anima (`Anima2.9B v1`, version id
3289643, a 5 GB file of another name), three are NoobAI and the rest Illustrious. The file on
this disk is v24. So the owner's "it is an Anima model" and the header's "it is SDXL" are about
two different files of one model page, and the base that decides which LoRAs apply to this one
is Illustrious.

Asked for every ComfyUI checkpoint and LoRA here that has a sha256 (85 files, one request
each):

| | Known to Civitai | Unknown |
| --- | --- | --- |
| checkpoints | 37 | 11 |
| LoRAs | 35 | 2 |

Where hoard already has a base (flux, wan, zimage, krea2: 41 files), Civitai's answer is a
refinement of the same architecture every time, with no contradiction, and it is finer in
exactly the way LoRA applicability is: `Flux.2 Klein 4B`, `Flux.2 Klein 9B` and `Flux.2 D`
under hoard's one `flux`; `ZImageTurbo` against `ZImageBase`; `Wan Video 2.2 I2V-A14B` against
`Wan Video 14B i2v 720p`; `LTXV`, `LTXV2`, `LTXV 2.3`. The label is the uploader's and is
sometimes loose: a Gemma text-encoder LoRA is filed under `LTXV 2.3`, `umt5_xxl` under
`Wan Video`, and two LoRAs are typed `Checkpoint`. The 13 unknown are repackaged vendor files
(`*_int8_convrot`, the Klein base fp8 files) and one private LoRA.

The only offline lineage signal is the owner's own filing: `checkpoints/pony/`,
`checkpoints/Illustrious/`, `loras/anima/`. Base inference reads no path at all
(`infer/base.rs:73`, `infer_base`: tensors, metadata, config).

**F10. A LoRA's module names are its host's tensor names.** The 16 baseless LoRAs of 37:
anima 5 (`lora_unet_blocks_0_cross_attn_*`; four also carry `modelspec.architecture:
anima-preview/lora`), qwen-image 3 (`transformer_blocks.N.attn.add_k_proj` with `img_mlp`),
ltx 2 (`adaln_single`, `transformer_blocks.N.attn1`), minimax 2 (`base_model` metadata names
the host), z-image 2 (`layers.N.attention.to_k` with `adaLN_modulation`), krea2 1
(`text_fusion.layerwise_blocks`), and one Gemma text-encoder LoRA. `metadata_rules`
(`infer/base.rs:168`) reads `ss_base_model_version`, `general.architecture`,
`clip.projector_type` and `ss_sd_model_name`, and not `modelspec.architecture`, `model_type`
or `base_model`, which between them name the base of 9 files here.

**F11. A VAE belongs to a latent space, not to a family.** `Wan2_1_VAE_bf16`, `wan_2.1_vae`
and `qwen_image_vae` are the same 194 tensors. 13 of 15 VAEs have no base; only two say what
they are in metadata (`Flux.1-AE`, `HunyuanVideo15VAE`).

### Smaller things

**F12.** `nvidia/NVIDIA-Nemotron-Nano-9B-v2-NVFP4/model.safetensors (2 files)`: `set_labels`
(`catalog/scan/plan.rs:290`) disambiguates over every set found by name, including the
eight-placeholder set that `unit_roles` then refuses to anchor, so the one surviving row wears a
suffix for a collision nobody can see. The index document already says which files are the
model; `named_sets` builds a second set from the same stem anyway.
**F13.** The four LTX latent upscalers carry `base:any` (`infer/base.rs:86`, every `Upsc` with
no base) and so plug into SDXL, Flux and Wan. Their header says otherwise: metadata `config`
names `"_class_name": "LatentUpsampler"` and the first convolution takes 128 channels, where
RealESRGAN's takes 3.
**F14.** `gpt-oss-120b` reads `int8`: `infer/precision.rs:135-147` reads `quant_algo`, `format`
and `bnb_4bit_quant_type`, and the config's only signal is `quant_method: "mxfp4"`.
**F15.** `GET /api/v1/artifacts/{id}/pull-command` prints the blob path in `path`
(`api/artifacts.rs:209` takes `row.abs_path`) while every screen prints
`common.rs:334` `shown_path`.
**F16.** A shard set's trigger words are read from a shard's file name: `model of`.
**F18.** The loadout feature cannot be started from the web UI. `web/js/loadout.js:5`
(`loadLoadout`) adopts the first loadout the server lists and never creates one;
`app.js:427` answers `L` and `ADD TO LOADOUT` with the toast `no loadout on this node` when
there is none. `GET /api/v1/loadouts` on the owner's server answers `[]`, so on this machine
every way into the feature is a dead end, and nothing on screen says what a loadout is. One
exists only after `POST /api/v1/loadouts` or `hoard loadout import`.
**F17.** The shard-set `hf` line always adds `--include '<stem>.<ext>.index.json'`; a GGUF
split and a parquet set have no such file. Harmless, and noise.

### Found while the waves ran, and still open

**F19. A card's trigger words bleed across a category folder.** `catalog/naming.rs:72-79` keeps
one revision-wide card, the shallowest README, and `catalog/scan/plan.rs` hands
`plan.context.frontmatter()` to every artifact of the revision. `comfy/text_encoders` is one
repository and its only README sits in `gemma-3-12b-it-qat-q4_0-unquantized/`, so
`ministral-3-3b.safetensors` carries the trigger words `gemma3, gemma, google`. The rule to
adopt is the one `config_for` and `quant_config_for` already follow: a card describes only
files at or below its own directory. A stored trigger changes, so it needs a bump.
**F20. An incomplete set still pulls and pins as if it were one file.** `State::Triage` has
bytes, so a loadout pin on gpt-oss-120b resolves to `Present` and apply links 1 of 15 shards;
and `api/common.rs::hf_scope` reads `shard_count <= 1` as a single file, so the copied line
re-fetches the shard already held rather than the set's globs, which is the command that
would complete the download through hoard. A correct fix needs an `incomplete` flag on
`SearchRow`.
**F21. The rail folds a lineage into its architecture.** The family-root tally shows one
`sdxl 5` row; `sdxl pony` and `sdxl illustrious` are in the BASE column and answer
`base:sdxlpony`, and are not rail rows. Left as the rail's existing behaviour unless the owner
wants lineages clickable.

### Closed after the six waves (2026-09-18)

F19 closed by `7146f16` (a card covers only the files at or below it). F20 closed by `cd24004`
(`incomplete` on the row, the set's globs in its pull line, a pin on it never present, a
rejection stored on the record). The confirm side closed by `5af2c4d` (a verdict settles the
paths it clears; the gate is `needs_triage`, the predicate the scan writes from). Still open:
F21 (lineages as rail rows, the owner's call); a set completed through the mirror stays
incomplete until a rescan (needs the declared total stored); un-rejecting a noise path cannot
reach its record without a new index (a bump); the summary counts a base-only verdict and a
retired remote record that the queue does not (pre-existing; whether the three numbers must
agree unconditionally is the owner's call).

### Checked and left alone

- `nvidia/diffusiongemma-26B-A4B-it-NVFP4` in triage as `unk`: its architecture is
  `DiffusionGemmaForBlockDiffusion`, which the previous plan's D11 deliberately does not take
  for a causal LM. Triage is where an unrecognised architecture belongs; the owner confirms it
  once there.
- `text_encoders/` files reading `ckpt` or `llm`: raised by `comfyui-model-taxonomy.md`,
  declined twice, not part of this review's verdict. Not reopened.
- `hidream_o1` is a fused file the taxonomy document already explains.

## Decisions

"Bump" means a `catalog::FORMAT_VERSION` bump under the Compatibility section of `CLAUDE.md`.
There are two bumps in this plan, one in Wave 3 and one in Wave 3b.

### D1. The page describes the artifact that was opened

`GET /api/v1/repos/{type}/{org}/{name}` takes `artifact=<id>`. When the id names an artifact of
that repository it replaces `pick_primary` for the header role and state, `meta`, `facts`,
`paths`, and `pull`, which becomes `pull_commands(.., Some(PullArtifact::from(row)), ..)`: the
same line `Y` copies. When no `revision` is asked for, the page opens on the artifact's own
revision, so Ministral's shard set opens on `613bd8d` with its 17 files. FILES marks the rows
whose `artifact_id` is the opened one (the field is already on every file row). Without
`artifact`, or with one the repository does not hold, the page behaves as today.

Rejected: a separate artifact page. The folder view is right for a model repository, the owner
said so, and one page that knows what was opened keeps both.

Cost: none stored. Wave 2.

### D2. A category folder offers one file; a model repository offers the file and the whole

`PullCommands` gains `repository: Option<String>`: the repository-level `hf` line, present
unless the repository is a category bin (F2: a loose repository whose org is its source's
name). The PULL box prints the opened artifact's line first and, when present, the repository
line under a `whole repository` caption; COPY copies the first. For `comfy/loras` there is one
line and it names one LoRA. For an hf-cache repository or an LM Studio folder there are two.
The generated card of a bin stops printing `hf download comfy/loras` under "Pull it" and
prints nothing there.

When the opened artifact is an LLM with a projector in the same repository, or the projector
itself, the first line names both files (`hf download <repo> 'model.gguf' 'mmproj.gguf'`; the
CLI takes several). Compat already resolves exactly that pair
(`api/compat.rs`, the `Proj` rule of the previous plan's D8), so the rule is one lookup and
not a new concept.

Download groups are loadouts and no second grouping concept is built. What is built is making
the one that exists true and reachable: `hf_script` calls `pull_commands` per item, so a group
script carries shard globs, `--revision` and `--repo-type`, and D10 lets the UI start a
loadout at all. The drawer gets no script button. The owner asked, and the answer is that the
spec's group download is `APPLY`, run on the node that wants the files: it pulls what is
missing from peers and writes the links (`api/loadouts.rs`, `run_apply`; applying to another
node is a report by design, `report_only`, because only the node that owns the disk can pull
to it). The script serves only a machine that has the `hf` CLI and no hoard, and the export
route and `hoard loadout export` already reach it.

Cost: none stored. Wave 2.

### D3. A lone shard is an incomplete set, and it queues rather than vanishes

`MIN_MEMBERS` stops gating how many members are present: a declared set with at least one
present member is a set, and so is a lone file whose name says `-of-<total>` with a total of
two or more. `is_incomplete` then says 1 of 14, the artifact is named as a set
(`openai/gpt-oss-120b/` `model.safetensors`), its state is TRIAGE and its reason reads
`1 shard present, incomplete`. A `-00000-of-00001` file stays a single file, as today.

The owner leaned towards ignoring it. It queues instead, for three reasons: two of fourteen
already queues, and one of fourteen behaving differently is the defect; the 4.1 GB is real
disk the DISK PRESSURE reader wants to see; and the queue's `reject` already makes a path
disappear from every later scan, so ignoring it is one keypress and the owner's decision rather
than the scanner's. Its files keep serving on the Hub face, which is what lets a client resume
the download through hoard.

Rejected: reading `.incomplete` files as the signal. They are the CLI's private state, they
are absent after a `--local-dir` download, and the index document is the authority on what a
model is.

Cost: bump (a stored name, state and flag change). Wave 3.

### D4. `any` is selected only by `any`

`selects` compares `is_any()` on both sides first: an `any` bucket is active only when a
positive `base:` token holds `any`, and a token holding `any` activates no other bucket. The
prefix rule for every other pair is unchanged. `Base::matches` is not touched, because compat
needs the wildcard.

Cost: none stored. Wave 1.

### D5. The sniffer answers the rules' questions over the whole header

`TensorSummary` gains `markers`: the rule fragments and exact keys found among **all** tensor
names, computed by the sniffer while it still holds them. The vocabulary is derived from the
rule tables (every `KeyMatch::Fragment`, every exact key of D6), not kept by hand, so a rule
cannot name something the sniffer does not look for; a test asserts it. `has_fragment` reads
`markers`. Shape-bearing rules name the tensor they need (`attn2.to_k`), and the first tensor
matching each such name earns a sample before the keyword pass and the stride do.
`SAMPLE_KEYWORDS` is deleted. Matching 100-odd patterns against up to 9,000 names is one
Aho-Corasick pass per header (`aho-corasick` is already in the lock file through `regex`;
declare it in the workspace).

Inference always runs straight after a fresh sniff (`scan/plan.rs:499`, `ingest.rs:369`,
`hoard-net/src/peer.rs:724`), never from a stored summary, so nothing else has to change to
feed it.

Rejected: raising `MAX_SAMPLES`. It moves the cliff. Rejected: handing inference the full key
list. The summary is the one interface every rule is written against and tested through.

Cost: bump (stored summary shape). Wave 3.

### D6. Families are named by exact keys, from ComfyUI's table, for hosts and LoRAs alike

`BASE_RULES` gains an exact-key tier ported from `model_detection.py`, each row citing the
line it came from: the ten families of F8 plus the ones hoard already has, so both tiers agree
where they overlap (they do on all 23 today). A second column per family gives the module
fragments a LoRA of that family carries (F10), matched after the LoRA suffixes and the
`lora_unet_` spelling are normalised. `metadata_rules` also reads `modelspec.architecture`,
`model_type` and `base_model` through `canonical_base`, keeping a label only when it spells
the key (the previous plan's D6).

VAEs get the metadata rules and no tensor table (F11): two files sharing every tensor cannot
be told apart, and naming one of them `wan` would be a guess.

Cost: bump (stored bases). Wave 3.

### D7. BASE is what decides which adapters apply, and Civitai is asked for it by hash

The owner's ruling: Pony and Illustrious are valid bases, and a correct base matters because
it limits the applicable LoRAs. So BASE is the finest name that is known, stored under its
architecture as a prefix: `sdxlillustrious`, `sdxlpony`, `sdxlnoobai`, `flux2klein9b`,
`zimageturbo`. `Base::matches` is already a two-way prefix test, so with no new field
`base:sdxl` finds every SDXL lineage, a Pony LoRA resolves against a Pony host or a host known
only as `sdxl` and not against an Illustrious one. The rail keeps them as separate rows
(`sdxl`, `sdxl pony`, `sdxl illustrious`) and no brand row merges them: the owner wants Pony
and Illustrious clickable as bases, and `search/brands.rs` already pins that no anchor is one
of the Stable Diffusion families. The BASE column prints the label, `sdxl illustrious`, which
spells the stored key and so survives the label rule.

Three sources, strongest first, each recorded as evidence:

1. The header (D5, D6): proves the architecture. Nothing overrides it; a lookup that names
   another architecture is recorded as a contradiction and loses.
2. A Civitai lookup by sha256: names the lineage or variant. A closed table maps Civitai's
   `baseModel` strings onto hoard keys under the right architecture; a string the table does
   not know is kept as evidence and names nothing.
3. The file name or a folder on the path spelling a lineage from the same table, at
   `FILENAME_HINT` weight, only as a refinement of a proven architecture. This is what an
   offline machine and a private file get. A folder never names an architecture by itself.

The lookup is a pass of its own, after hashing, because it needs the sha256:

- Off unless `[enrich] civitai = true`, and never run while `[mirror] offline = true`. It sends
  the hashes of the owner's files to a third party; that is the owner's switch to throw, and
  the config comment says so.
- One request per sha256, ever. Answers, including "unknown" with the date it was asked, live
  in `<data_dir>/civitai.redb` beside `digests.redb`, outside `native/`, so a format bump
  re-reads them from disk and asks nothing. Unknowns are asked again after 30 days.
- Sequential and rate-limited, a job on the JOBS page like hashing, cancellable the same way.
- It writes a base only through the catalog's override-shaped path, moving counts through
  `DerivedShift` and logging through `log_changes`, per `CLAUDE.md`.
- The HTTP client lives in `hoard-mirror`, the lowest crate that has one; the mapping table
  and the evidence type live in `hoard-index`.

The same answer carries the model's name, version and `trainedWords`. Showing the Civitai
name in the inspector and taking trigger words from it are obvious follow-ups and are not in
this wave.

Rejected: lineage as its own field. The prefix does the work of a second field with none of
the compat, grammar, rail and digest changes one would need. Rejected: scraping the web page.
The by-hash route is a documented JSON API and needs no key for public models.

Cost: a stored base changes, so the mapping and the file-name rule ride in Wave 3's bump; the
lookup pass is Wave 4 and bumps nothing.

### D8. What base identification deliberately does not do

No tensor table for VAEs (F11). No modality field. No table of display labels beyond the
lineage table of D7. No lookup that is on by default.

### D10. A loadout can be started from the UI, and says what it is

See F18. The `LOADOUT` button and `L` create a loadout named `loadout` on first use instead of
toasting `no loadout on this node`; the empty drawer says in one line what it is for
(`a checkpoint and the adapters you use with it; L adds the row under the cursor`). Wave 2,
with D2's script fix.

### D9. Small things, decided

- A declared set claims its stem: when an index document names a set in a directory,
  leftover files of the same directory, stem and extension form no second set. The Nemotron row
  reads `model.safetensors`. Bump, Wave 3.
- An upscaler is `any` only when it is a pixel upscaler: not when its metadata `config` names
  a latent class or its first convolution takes more than four channels. The LTX upscalers
  lose `any`, and gain `ltx` through D7's file-name rule,
  otherwise no base. Bump, Wave 3.
- `quant_method` names the precision when it is itself a scheme (`mxfp4`), and stays ignored
  when it names a producer (`compressed-tensors`, `modelopt`, `bitsandbytes`). Bump, Wave 3.
- A shard set's trigger words come from its label, never from a member's file name. Bump,
  Wave 3.
- `pull-command` `path` is `shown_path(row)`. Wave 2.
- The index `--include` is written only for an extension that has index documents
  (`safetensors`, `bin`). Wave 2.

## What is not being built

- **A second grouping feature beside loadouts** (D2).
- **Lineage as a field, or a lookup that is on by default** (D7, D8).
- **A tensor table for VAEs** (F11).
- **An `INCOMPLETE` state or an ignore rule in the scanner** (D3); triage with a reason and
  `reject` cover both.
- **True substring search.** `wave/find-and-pull` made free text a word-prefix match and
  documented that `SoulsMix` inside `loxsSoulsMixPerfectDoll` does not match. A substring
  index is an n-gram field and a measurable cost at a million artifacts; it is a separate
  decision with its own number.

## Waves

Each lands on `master` as one squashed commit through the loop in `CLAUDE.md`.

### Wave 0: land `wave/find-and-pull`

Done and verified (1,077 tests, release asset test, live check on port 7199). Waiting on the
owner's word, because it is a commit to `master`.

### Wave 1: `any-is-not-a-wildcard` (no bump)

D4. `crates/hoard-index/src/search/facets.rs` and its tests. Acceptance on this cache:
`q=base:flux` returns exactly one active base bucket; `q=base:any` returns exactly one.

### Wave 2: `opened-artifact` (no bump)

D1, D2 and the two Wave 2 items of D9. `api/repos.rs`, `api/common.rs`, `api/artifacts.rs`,
`api/loadouts.rs`, `web/js/{app,detail,api,loadout}.js`, `docs/api/hoard-api.md`,
`docs/design/ui-spec.md` (PULL box, loadout drawer). Release asset test. Acceptance on this
cache: opening `Z-Detail-Slider.safetensors` shows its own facts (21.3 MB, `zimage`) and one
PULL line naming `zit/Z-Detail-Slider.safetensors`; opening Ministral's `model.safetensors`
lands on `613bd8d` with 17 files and a first line carrying `--revision`; opening
`Qwen3-VL-8B-Instruct-Q8_0.gguf` copies a line naming the GGUF and its `mmproj`; a loadout
holding Ministral's shard set exports the two `--include` globs and `--revision`.

### Wave 3: `whole-header-inference` (a bump, 12 to 13)

D5, D6, D7's stored half (the prefix values, the mapping table and the file-name rule), and
the two inference items of D9 (the latent upscaler, `mxfp4`). One subsystem: `sniff/` and
`infer/`. `sniff/{mod,safetensors,gguf}.rs`, `hoard-core/src/artifact.rs`,
`infer/{rules,base,precision}.rs`, `search/brands.rs` (the `sdxl` group),
`docs/native-catalog.md`, `docs/api/hoard-api.md`. Fixtures are synthetic headers built from
the key lists recorded in this document, never real weights.

Acceptance on this cache, after the rebuild (a re-sniff, not a rehash, through the digest
memo):

| Reading | Today | After |
| --- | --- | --- |
| safetensors checkpoints with a base | 23 of 55 | at least 44 |
| LoRAs with a base | 21 of 37 | at least 34 |
| `base:sdxl` | 2 | 5, of which `Illustrious` 2 and `Pony` 1 by folder name before Wave 4, by Civitai after |
| `flux-2-klein-4b` PLUGS INTO total | 46 | 42 (no LTX latent upscaler) |
| `prec:mxfp4` | 0 | 1 |

### Wave 3b: `honest-sets` (a bump, 13 to 14)

D3 and the grouping items of D9 (a declared set claims its stem; a set's trigger words come
from its label), plus one item the review of Wave 2 raised: the tool layout a loose source was
recognized as (`discover/loose.rs::layout`) is computed at discovery and thrown away, so
`is_category_bin` has to guess from `org == source` and also catches a per-model folder
directly under a plain loose source. The source record stores the recognized layout and the
server reads it. One subsystem: `shard.rs` and `catalog/scan/`. It bumps separately from
Wave 3 because one implementer session holds one subsystem; a bump costs a re-sniff, measured
at 20 seconds on this cache with hashing off.

| Reading | Today | After |
| --- | --- | --- |
| `openai/gpt-oss-120b` | LOCAL, named as the repository | TRIAGE, `1 shard present, incomplete` |
| triage, on the shelf, the queue and stats | 22 | 23, all three |
| Nemotron NVFP4 name | `model.safetensors (2 files)` | `model.safetensors` |

### Wave 4: `civitai-lookup` (no bump)

D7's lookup pass: the config switch, the sidecar, the job, the client in `hoard-mirror`, the
evidence row. Tests run against `wiremock`, never the real service. Acceptance on this cache
with the switch on: 72 of the 85 hashed ComfyUI checkpoints and LoRAs carry a Civitai evidence
row; `oneObsession_v24` and `loxsSoulsMixPerfectDoll_v30` read `Illustrious`,
`cyberrealisticPony_v160` reads `Pony`; a second run makes no request; with
`offline = true` the job does not start.

### Sequencing

Wave 0 first: it is finished. Wave 1 next: ten lines, no rebuild, a visible lie on every
filtered page. Wave 2 before Wave 3, because the owner's complaint was the journey and Wave 2
is the journey; it reads nothing Wave 3 produces. Wave 3 and Wave 3b each alone with a bump, then Wave 4,
which reads the sha256 and the architecture Wave 3 leaves.
