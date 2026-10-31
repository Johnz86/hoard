# The acquire path, naming, the rail, compat and triage: the resolution plan

Historical decision brief. These waves landed; later acquire-path work refined them. See
[current design status](README.md); do not repeat the catalog format bump below.

Status: decided. This document answers the owner's rejection of the brand-rail delivery
("this implementation is not good enough") and the seven items `catalog-consistency-review.md`
confirmed after it. It supersedes `catalog-consistency-review.md` as the statement of what is
wrong, and it corrects `brand-rail-and-hashing-refactor-plan.md` in two places, both named
below (Decision 1 of that plan on brand labels, and its "pairing a projector to its sibling
host" deferral). Where earlier design documents disagree with this one, this one wins.

It is not a wave plan. It is the brief the wave plans are written from, in the shape
`brand-rail-and-hashing-refactor-plan.md` had. `.claude/skills/hoard-write-plan/SKILL.md` says
what the plans themselves must carry.

No application code was changed to produce it. Every claim about current behaviour was read out
of the tree at `5de7193` (catalog `FORMAT_VERSION` 11) and checked against the running server
(`target/debug/hoard serve --config hoard.toml`, pid 1685633, catalog built 19:54-20:02 UTC on
2026-09-16 over this workstation's real caches: 233 artifacts, 1.6 TB). Every number is from
that server's `/api/v1` and Hub-face responses, from the real files on disk, or from the source
at the cited line. The commands are given where they are used, so the next session can re-run
them.

## Why this document exists

The owner used the shipped rail and rejected it with three observations: `GEMMA` is uppercased
while `diffusion_gemma` sits separately and unmerged; `NVIDIA NEMOTRON` carries a company name
no other row carries; and picking `NVIDIA NEMOTRON` shows two different files that both read
`nvidia/model/model`. A follow-up review confirmed those and found four more: the pull command
is never scoped to the selected file, the rail mixes two label conventions, org visibility
depends on source kind, and `PLUGS INTO` crosses modalities through `base:any`.

This plan started from the owner's framing rather than from that list: someone wants to browse,
find, and then actually acquire a specific artifact with `git clone` or `hf download`. Walking
that journey end to end, for a checkpoint, a LoRA, a GGUF quant, a sharded model and a projector
that needs pairing, turned up something the seven items sit on top of: **the acquisition step
does not work for most of this cache.** The `git clone` line is dead for every repository here,
and the Hub face cannot serve any file from a loose source, because a loose repository's
revision carries no sha and the mirror therefore asks huggingface.co instead of the disk. Everything
else in this plan is real and worth fixing, but that comes first, because a catalog that names a
file beautifully and then cannot hand it over has not done its job.

## The journey, walked

Each row is one thing a user does, what the UI shows for it today, and whether that is true.
The artifact ids are this catalog's.

| Step | What the user sees | True? | Finding |
| --- | --- | --- | --- |
| Open the index | 233 rows; `BASE ARCHITECTURE` headed by `any 45`, then `QWEN 16`, `zimage`, ... `GEMMA 7`, `NVIDIA NEMOTRON 5`, ... `diffusion_gemma 1` | The `any` row is not an architecture; two label conventions; `diffusion_gemma` outside `GEMMA` | F9, F10, F12 |
| Click `NVIDIA NEMOTRON` | 5 rows: three `nvidia/...`, two bare file names, two of them `nvidia/model/model` (7.8 GB LOCAL, 320 B TRIAGE) | The 320 B "model" is eight empty placeholder files; the name collision is a no-op tiebreaker | F5, F6, F7 |
| Read a row's PREC | `flux-2-klein-9b-fp8.safetensors` reads `fp32`; `Qwen3-8B-NVFP4` reads `fp32`; `mmproj-...-F16.gguf` reads `f16` beside `fp16` rows | Precision is the dtype with the most *tensors*, which is the scales, not the weights | F13, F14 |
| Read a row's BASE | `qwen3vl_merger`, `qwen3_5_moe`, `StableDiffusionXLPipeline`, `nemotron_h_moe`; the rail's member list says `qwen35moe` | The column prints the source's raw spelling, which is not what `base:` accepts | F11 |
| Open the inspector on an LLM | `PLUGS INTO`: four image upscalers | `base:any` matches every host regardless of role pairing | F15 |
| Open the inspector on `mmproj-Qwen3-VL-8B-Instruct-F16.gguf` | `PLUGS INTO`: `qwen3-14b-unsloth-bnb-4bit`, `Qwen3-VL-8B-Instruct-Q8_0.gguf`, `Qwen3-8B-NVFP4`, `Qwen3-0.6B` | Three of four are wrong; the right one is the file beside it in the same folder | F16 |
| Press `Y` on any row | Toast: `copied: git clone http://127.0.0.1:7099/<org>/<repo>` | The git face answers 404 for every repository in this catalog; the command is never file-scoped | F1, F2 |
| Run the `hf download` line for an hf-cache repo | Downloads the whole repository | Works, but every quant, shard and precision in the repo, not the file chosen | F2 |
| Run `hf download comfy/loras Z-Detail-Slider.safetensors` | `Error: File not found in repository` | The file is on disk, hashed, indexed and listed by `/api/models/comfy/loras` | F3 |
| Run `hf download lmstudio-community/Qwen3-VL-8B-Instruct-GGUF mmproj-...` | `httpx.ReadTimeout` after 10 s | hoard fetched the bytes from huggingface.co at 4.85 MB/s instead of serving the local file | F3 |
| Press `S` on a row | Toast: `served X → http://.../null/X` | Nothing happened; the key is a placebo | F20 |
| Open a repo page | Header meta prints `.../blobs/cedcf7e8...`; PULL box shows git and hf, COPY copies git | Blob paths are content addresses, not names; git is dead here | F1, F8 |
| Look at DISK PRESSURE | `80.8 GB reclaimable — 4 duplicate blobs` | 79 GB of that is one blob reached through two snapshot symlinks; the real figure is about 1.9 GB and 2 blobs | F18 |
| Look at the triage count | `SAVED SHELVES (18)`, `triage queue 18`, `/api/v1/triage` lists 21, `state:triage` answers 22, `/api/v1/stats` says 21 | Three predicates for one word | F17 |

## Findings

Grouped by what they break. Each carries the evidence and the exact root cause.

### The acquire path

**F1. The `git clone` command is dead for this entire cache, and it is the one `Y` copies.**
`crates/hoard-server/src/api/common.rs:461-472` builds `git clone {url}/{reference}` for every
artifact; `web/js/app.js:453-461` copies `command.git` on `Y`; `web/js/detail.js:289-294` copies
`pull.git` on the repo page's COPY button; the generated card (`/api/v1/repos/.../readme` for a
loose repo) prints it under "Pull it". The git face serves only a repository held as a git
clone (`crates/hoard-server/src/git/mod.rs:167-175`, `is_clonable`; `docs/api/hub-compat.md:139-141`
documents the 404 for an HF cache snapshot or loose folder). This catalog's repositories are of
kind `hf-cache` and `loose` only (`GET /api/v1/repos` lists no `git` kind). Measured:

```
GET /nvidia/NVIDIA-Nemotron-Nano-9B-v2-NVFP4/info/refs?service=git-upload-pack  -> 404
GET /comfy/loras/info/refs?service=git-upload-pack                              -> 404
GET /lmstudio-community/Qwen3-VL-8B-Instruct-GGUF/info/refs?service=git-upload-pack -> 404
```

So the primary command on every page, and the only one a keyboard user can reach, fails for
100% of what is indexed here.

**F2. The `hf download` command is never scoped to the file.** `pull_commands` accepts `path`
and puts it on the struct without using it; `api/artifacts.rs:200-203` computes the right
per-artifact path and nothing renders it. The per-file spelling already exists in the tree and
is not reused: `api/loadouts.rs:803-822` (`hf_script`) writes `hf download <repo> <path>` per
loadout item. For a sharded model the path is one shard, which is not the model either.

**F3. The Hub face cannot serve a file from a loose source; it either 404s or fetches the same
bytes from the internet.** This is the finding under everything else in this section.

- `crates/hoard-index/src/catalog/scan/revisions.rs:52-60`: `discovered_revision` stores
  `sha: None` for `RevisionName::Synthetic`, which is every loose folder's one revision.
  `discover/loose.rs:431-454` defines `synthetic_sha` over the folder's content triples, and
  nothing in the workspace calls it (the only other `synthetic_sha` is `hoard-net`'s own).
- `crates/hoard-mirror/src/resolve.rs:640-668`: `resolve_revision` asks the index first
  (`index_revision`, lines 716-725, returns `revision.sha`), gets `None`, and falls through to
  the upstream branch because the config's `[[mirror.proxy]] repo = "*/*" allow = true`.
- For `comfy/loras`, huggingface.co has no such repository, so the answer is 404; `refine`
  (`hub/files.rs:75-82`) turns that into `EntryNotFound` because the index does hold the repo.
  For `lmstudio-community/*`, `microsoft/TRELLIS.2-4B` and `facebook/dinov3-*`, huggingface.co
  has repositories of those names, so the mirror resolves *their* sha, cannot match it to the
  local revision (`index_repo.rs:307-335`, `locate` -> `revision_by_sha`), and proxies the bytes.
- Measured on this server: `GET /comfy/loras/resolve/main/Flux_2-Turbo-LoRA_comfyui.safetensors`
  404 `EntryNotFound` (also `comfy/upscale_models/...`, `comfy/vae/...`, `facebook/dinov3-.../model.safetensors`);
  `GET /lmstudio-community/Qwen3-VL-8B-Instruct-GGUF/resolve/main/mmproj-Qwen3-VL-8B-Instruct-F16.gguf`
  200 with `x-repo-commit: 2e40238691f7...` (an upstream commit; the local revision has none),
  1,159,029,824 bytes in 238.8 s at 4.85 MB/s, after which `data/repos` held 1.2 GB it did not
  hold before and `/api/v1/stats` `cache.bytes` went from 0 to 1,226,291,112. The identical file
  is at `/home/johnz86/.lmstudio/models/lmstudio-community/Qwen3-VL-8B-Instruct-GGUF/` and reads
  at 1.3 GB/s. An hf-cache blob over the same loopback served at 382 MB/s.
- `GET /api/models/<loose repo>/tree/main` answers 404 `RevisionNotFound` (`hub/meta.rs:198-218`,
  `local_sha`), which is what makes `hf download --include` fail with "Revision not found".
- `hf download comfy/loras Z-Detail-Slider.safetensors` with an isolated `HF_HOME`: `Error: File
  not found in repository`. The same command against `nvidia/NVIDIA-Nemotron-Nano-9B-v2-NVFP4`
  with `config.json model-00003-of-00010.safetensors` succeeded, which is the control.

Two of this workstation's three sources are loose. The block cache my probes filled is under
`./data/repos` (git-ignored) and is ordinary evictable cache content.

**F4. A file with no tensors becomes an artifact.** `nvidia/NVIDIA-Nemotron-Nano-9B-v2-NVFP4`
ships `model-00001-of-00010.safetensors` and `-00002-` as real shards (7.8 GB together, exactly
the `total_size` its index declares) and `-00003-` through `-00010-` as one shared 40-byte
blob whose whole content is `{"__metadata__":{"format":"pt"}}`: a safetensors header with zero
tensors. `scan/plan.rs:70-76` (`creates_artifact`) admits any artifact-format file that is not
pointer bytes; `unit_roles` (`plan.rs:314-344`) anchors a set on any file whose format is an
artifact format. So the eight placeholders form a second set and a second artifact, `a_17`:
320 B, `role: llm` at 0.9 from `config.json` alone, `state: triage` because the set is
incomplete (8 of a declared 10).

### Naming and identity

**F5. The `model/model` collision is real and its tiebreaker cannot break it.** The index
document names shards 1-2, so `shard.rs:151-185` (`declared_set`) claims them; the leftover
placeholders parse as `<model>-<n>-of-<10>` and `named_sets` (`shard.rs:196-235`) makes a
second set with the same `stem` in the same (empty) directory. `naming.rs:119-127`
(`shard_label`) labels both `model`; `naming.rs:129-144` (`disambiguate_labels`) appends each
set's stem, which is also `model`, so both read `model/model`. F4 removes this instance; the
tiebreaker stays wrong for any two sets that share a stem and a directory.

**F6. NAME carries five conventions.** From `GET /api/v1/search?q=&limit=500`:

| Artifact | NAME today | Rule that produced it (`naming.rs:146-168`) |
| --- | --- | --- |
| `a_6` | `Qwen/Qwen3-0.6B` | hub repo holding one artifact: the repo name |
| `a_8` | `Qwen/model.safetensors` | hub repo holding two artifacts: the path inside the repo |
| `a_9` | `Qwen/speech_tokenizer/model.safetensors` | same rule, same repo (`Qwen3-TTS-12Hz-1.7B-VoiceDesign`, never shown) |
| `a_14` | `mistralai/model` | a shard set at the repo root: the stem |
| `a_13` | `mistralai/Ministral-3-14B-Instruct-2512` | the same repo's `consolidated.safetensors`, named by the revision where it was alone |
| `a_218` | `NVIDIA-Nemotron-3-Super-120B-A12B-GGUF` | a loose folder holding one sharded model: the folder name, no org |
| `a_210` | `GLM-4.7-Flash-Q8_0.gguf` | a loose folder holding one file: the file name, no org |
| `a_44` | `Wan2_1-I2V-14B-480p_fp8_e4m3fn_scaled_KJ.safetensors` | a ComfyUI folder: the file name, no org |

The repository name, which is the thing a Hub user recognises, appears only when the repo holds
one artifact, which is exactly when it is least needed. A loose model's org (`lmstudio-community`)
is known to the catalog (`repo: models/lmstudio-community/...`) and never shown. The
ComfyUI/LM Studio distinction between "folder name" and "file name" turns on whether the file is
sharded.

**F7. A name is chosen per revision and kept per artifact.** `store.rs:150-159`
(`names_better`, `naming_key`): more bytes wins, then the lexically smaller name. `a_13` is
`consolidated.safetensors` in both Ministral snapshots; in `main` it is the only artifact so it
reads as the repo, in `613bd8d` it sits beside the 4-shard `model` so it would read
`consolidated.safetensors`. The repo name won because `M` sorts before `c`. That is an accident,
not a rule.

**F8. Human-facing paths are blob paths for the HF cache.** `common.rs:234` prints
`abs_path` when there is one, and for an hf-cache entry that is the content-addressed blob:
the inspector shows `.../blobs/4c77b0f1717f...`, the repo header's meta line prints
`.../blobs/cedcf7e8... · 7.9 GB`, and PATHS ON DISK lists two blob paths with `symlink`. The
snapshot path (`.../snapshots/8556c91.../model-00001-of-00010.safetensors`) is what the walk
followed (`discover/hf_cache.rs:263`) and what a person recognises.

### The rail and the base vocabulary

**F9. Brand labels are free text, and the convention flips between rows.** `search/brands.rs:12-18`
carries a third column per anchor (`QWEN`, `GEMMA`, `LFM`, `NVIDIA NEMOTRON`). Every row a
brand does not claim prints its stored label in lowercase (`zimage`, `flux`, `mistral3`,
`clip_text_model`, `muse-glimmer`, `dinov3_vit`). The uppercase was meant as a "this is a group"
signal; the owner read it as inconsistency, and the one company name is the table's own
inconsistency with itself.

**F10. `diffusion_gemma` is outside `GEMMA`.** The base value is `diffusiongemma`; `brand_of`
is an anchor prefix match and the anchors are `gemma3`, `gemma4`. A brand is a product line as
its publisher names it; DiffusionGemma is Google's Gemma line. The union-not-prefix click rule
already makes merging it safe for correctness (see the previous plan's Decision 1).

**F11. The BASE column prints raw source spellings, some of which are not spellings of the
key.** `base_label` is the source's verbatim string (`infer/base.rs`, asserted at lines 400 and
524): `nemotron_h`, `qwen3_5_moe`, `qwen3vl_merger`, `StableDiffusionXLPipeline`,
`qwen3_tts_tokenizer_12hz`. The rail's member tooltip and `base:` accept the key
(`qwen35moe`, `qwen3vl`, `sdxl`). `qwen3vl_merger` is a projector topology, not a family; the
previous plan trimmed the key and left the label. `StableDiffusionXLPipeline` is a diffusers
class name. Three gemma-4 projectors read `—` because their GGUF carries no
`clip.projector_type` (read from the files: `general.file_type 32`, `general.name "Gemma 4 26B A4B"`,
no projector type).

**F12. `any` heads BASE ARCHITECTURE.** 45 of 233 artifacts carry `base:any` (the upscalers),
so the section's first and largest row names no architecture. `docs/api/hoard-api.md` says the
bucket exists so the facet's own query answers, which is right; its position is what misleads.

### Precision

**F13. PREC reads the dtype of the *scales*, not the weights.** `hoard-core/src/artifact.rs:232-249`:
`TensorSummary::precision` picks the dtype with the most tensors. Read from the real headers
(`data_offsets` give bytes; the tensor counts are what hoard stores):

| File | tensors by count | bytes | hoard says |
| --- | --- | --- | --- |
| `flux-2-klein-9b-fp8.safetensors` | F32 224, F8_E4M3 112, BF16 89 | F8_E4M3 92.5% | `fp32` |
| `flux2_dev_fp8mixed.safetensors` | F32 256, BF16 171, F8_E4M3 128 | F8_E4M3 81.8% | `fp32` |
| `wan2.2_i2v_high_noise_14B_fp8_scaled.safetensors` | F32 812, F16 689, F8_E4M3 407 | F8_E4M3 99.9% | `fp32` |
| `Wan2_1-I2V-14B-480p_fp8_e4m3fn_scaled_KJ.safetensors` | F16 821, F32 482, F8_E4M3 481 | F8_E4M3 97.0% | `fp16` |
| `z_image_turbo_nvfp4.safetensors` | F32 360, BF16 273, F8 180, U8 180 | U8 60.2% | `fp32` |
| `RedHatAI/Qwen3-8B-NVFP4` shard 1 | F32 486, F8 243, U8 243, BF16 139 | U8 66.7% | `fp32` |
| `unsloth/qwen3-14b-unsloth-bnb-4bit` shard 1 | F32 366, U8 366, BF16 74 | U8 57.9% | `fp32` |

Every quantised safetensors on this machine reads as the precision of its scale tensors. The
`prec:fp8` facet counts 6; the files above alone are 7 more.

**F14. `f16` and `fp16` are two facet rows for one thing, and an old projector reads `fp32`.**
`infer/precision.rs:17-21` takes the GGUF `general.file_type` name (`F16`) through
`Precision::new`, which lowercases, while a safetensors `F16` goes through
`Precision::from_dtype` to `fp16`. Facets: `fp16 27`, `f16 2`. `mmproj-model-f16.gguf`
(gemma-3-4b) carries no `general.file_type` at all (read from the file), so it falls to the
tensor-count rule and reads `fp32`.

### Compat and PLUGS INTO

**F15. Every host lists every upscaler.** `hoard-core/src/compat.rs:60-77`: same base or `any`,
host role against adapter role, nothing else. `Role::HOSTS` is `[Ckpt, Llm]` and every adapter
role plugs into both. Measured: `a_15` (Nemotron 30B LLM) COMPAT: 45 rows, all `upsc`;
`a_49` (`flux-2-klein-4b`) 46 rows: 1 LoRA and 45 upscalers; `a_44` (Wan I2V) 50 rows: 5 LoRAs
and 45 upscalers. The inspector shows the first four by size, so for an LLM the panel is four
upscalers and for a small checkpoint it is usually three.

**F16. A projector pairs by base prefix, and the prefix is wrong three times out of four.**
`Base::matches` (`hoard-core/src/role.rs:316-324`) is a bidirectional prefix test, so `qwen3`
matches `qwen3vl`. `a_221` (`mmproj-Qwen3-VL-8B-Instruct-F16.gguf`) lists `qwen3-14b-unsloth-bnb-4bit`,
`Qwen3-VL-8B-Instruct-Q8_0.gguf`, `Qwen3-8B-NVFP4`, `Qwen3-0.6B`; only the second is its host,
and it is the file beside it in `lmstudio-community/Qwen3-VL-8B-Instruct-GGUF`. `a_229`
(gemma-3-4b's projector) lists the 12B and `translategemma-4b-it` beside its own
`gemma-3-4b-it-Q4_K_M.gguf`. The previous plan deferred sibling pairing because the *scan*
commits one unit at a time; compat is answered at *query* time, where every candidate row
already carries `repo` (`api/compat.rs:104-114` filters candidates in memory).

### Triage

**F17. Three predicates answer to one word.** `store.rs:137-143` (`artifact_triage`):
`needs_triage(confidence) || incomplete` decides the entry state. `read.rs:534-572`
(`triage_queue`) queries `state:triage` and then re-filters on `needs_triage` alone, dropping
the incomplete set. `counters.rs:43-60` (`SummaryPart::of`) counts `needs_triage` alone for
`/api/v1/stats`. The seeded shelf is `role:?` (`userdata.rs:27`). On this catalog:
`role:?` 18, `/api/v1/triage` 21 (adds three `vae` at 0.505), `state:triage` 22 (adds `a_17`),
`/api/v1/stats` `triage` 21. The rail title, the shelf count and the queue disagree, and a queue
item does not say why it is there.

### Disk pressure

**F18. `80.8 GB reclaimable` is 79 GB of symlinks.** `counters.rs:53-58`: `copies =
path_count - 1`, `reclaimable = bytes * copies`. `dupe:true` answers `a_13` (15.7 GB: the same
blob in two snapshots), `a_15` (63.2 GB: thirteen shards, each reached from two snapshots),
`a_88` (1.7 GB: two real files, `Qwen-Image-Lightning-4steps-V1.0.safetensors` and
`ltx-2-19b-distilled-lora-384.safetensors`, byte-identical) and `a_23` (246 MB: an SDXL
`text_encoder` blob and a ComfyUI `clip_l.safetensors`). The honest figure is 1.9 GB and two
blobs. The stamp discovery records is taken with `std::fs::metadata` (`discover/walker.rs:306`),
which follows symlinks, so the inode of the bytes is already on every entry.

### Inference, narrowly

**F19. A vision transformer reads `llm` because its config has layers.** `infer/role.rs:289-305`
(`causal_language_model`): `architectures` naming `ForCausalLM`/`ForConditionalGeneration`,
*or* `model_type` present and `num_hidden_layers` present. `a_199`
(`facebook/dinov3-vitl16-pretrain-lvd1689m/model.safetensors`, architectures
`["DINOv3ViTModel"]`) reads `llm` at 0.9 with the detail "architectures declare a causal
language model", which they do not. The fallback is the whole reason.

### Small things that lie

**F20.** `S` toasts `served <name> → <url>/<org>/<name>` and does nothing (`app.js:469-472`,
`keys.js:136-142`); for a loose row the URL reads `.../null/<name>`.
**F21.** The results header prints `SORT USED` when no sort is chosen (`app.js:137-140`), while
the server default is `name` (`search/context.rs:177`), and that order is byte order:
`Z-Detail-Slider` precedes `ae.safetensors`.
**F22.** The JOBS row for the finished scan reads `refreshing search · 100% · 0 B · 0 B/s`
(`jobs.js:118-160`): the last phase's zeroes, not what the job did.
**F23.** SCANS has a BLOBS column that always reads 0 (`jobs.js:39,52`; `hoard-api.md`
documents the field as always zero).
**F24.** A seeded shelf is `everything on laptop` = `node:laptop` (`userdata.rs:27`); this node
is `workstation`, so the shelf reads `0` forever.
**F25.** The empty state's example query `compat:sdxl-base-1.0 role:lora` (`table.js:15`)
answers `InvalidQuery: compat target "sdxl-base-1.0" was not resolved` on this catalog; the
repository here is `stabilityai/stable-diffusion-xl-base-1.0`.
**F26.** `docs/design/ui-spec.md` glosses `LOCAL` as "held on a node, not served", but every
LOCAL entry answers on the Hub face; `SERVED` is what the mirror fetched. The label is fine;
the sentence is wrong.

### Checked and left alone

- The `text_encoders/` files reading `llm` with no base (`a_129`-`a_143`): a Qwen or Mistral
  text encoder is a language model, the SOURCE LAYOUT facet says where it was filed, and the
  earlier plan rejected changing this at the inference level. Not reopened.
- `a_9` (`Qwen3-TTS` speech tokenizer) reading `vae` from "an encoder and a decoder and nothing
  else": a neural audio codec is that. Left.
- The 32-bucket rail cap, the shelf counts, the token strip, the loadout drawer and the nodes
  page: nothing found.
- The hashing pass: the FORMAT 11 rebuild's scan job ran 19:54:34-20:02:26 (7 m 52 s) on a
  debug build with the digest memo, and reported progress. That delivery holds.

## Decisions

Each decision names its shape, the alternatives it rejects, and its cost. "Bump" means a
`catalog::FORMAT_VERSION` bump under the Compatibility section of `CLAUDE.md`: the catalog is
deleted and rebuilt from disk, never converted. There is exactly one bump in this plan, and
Wave 2 carries it.

### D1. A loose folder's revision carries its synthetic sha, so the index answers first

`discovered_revision` (`revisions.rs`) stores `synthetic_sha(...)` over the revision's entries
for `RevisionName::Synthetic` instead of `None`. The content triple is `(rel_path, fingerprint,
size)` as `loose.rs:431-454` already defines it; the fingerprint is the xxh3 discovery already
computes, so no byte is read that is not read today. The revision *key* stays
`RevisionKey::Named(default_ref)` for a synthetic revision, computed from `RevisionName` rather
than from `sha.is_some()` (`revisions.rs:43-48`), so a folder whose content changes updates its
one revision's sha in place rather than accreting a revision per change.

Effects, all by existing code paths: `index_revision` returns the sha, `local_answer` finds the
revision by it, `x-repo-commit` and `/api/models/<repo>` `sha` print it, `tree/main` and
`paths-info/main` answer, `hf download` with a file name or `--include` works for every loose
repository, and nothing the index holds is fetched from the internet.

Rejected: serving a held repository by name when its revision has no sha (skipping the sha in
`locate`). That leaves the Hub contract's `sha` field a lie and keeps the `tree` route broken.
Rejected: refusing upstream for any repository the index holds. Right for loose sources, wrong
for an hf-cache repo whose other branch is not cached; D1 makes the distinction fall out of
the data instead.

Cost: bump (a stored revision value changes for every loose repository). Wave 2.

### D2. A file with no tensors is not an artifact and anchors no set

`creates_artifact` is false, and `unit_roles` treats a set as unanchored, when the sniffed
`TensorSummary` has `tensor_count == 0` for a tensor container (safetensors, GGUF, pickle zip).
The file stays an entry, listed in FILES as `weights · 40 B · PRESENT`, claimed by no artifact.

On the NVFP4 repo the eight placeholders stop being an artifact and the repository holds one.
Its name is still decided from file names before anything is sniffed (`plan.rs:245-270`,
`revision_plan`, counts `names_an_artifact` over the chunk), so the placeholders still make the
revision "not single" and the surviving set is named by D3: `model.safetensors`, under the
prefix `nvidia/NVIDIA-Nemotron-Nano-9B-v2-NVFP4/`. One row, the repository visible, no
`model/model`. Teaching `revision_plan` to sniff before it counts is deliberately not done: the
count is decided once per revision so that every unit agrees, and a header read per file there
would undo the one-unit-at-a-time memory work.

Cost: bump. Wave 2.

### D3. Naming: one rule per source kind, and the repository always visible

The stored `org` and `name` keep their meaning; two things change in what is stored and one in
what is shown.

Stored:

- A shard set is named `<directory/><stem>.<extension>`, the same shape a single file has
  (`model.safetensors`, `unet/diffusion_pytorch_model.safetensors`), never the bare stem or the
  directory alone. The inspector's `files: 4 shards` fact already says it is a set.
- A loose file or set is always named by its path inside its folder repository, never by the
  folder (`NVIDIA-Nemotron-3-Super-120B-A12B-Q4_K_M.gguf`, not `...-GGUF`). The
  `naming.single` special case applies to hub and git repositories only, where the repository
  is a publisher's name for the model.
- `disambiguate_labels` breaks a collision by appending the member count, ` (2 files)` /
  ` (8 files)`, which is the one thing two sets sharing a directory and stem always differ in.
- `names_better` prefers, after "more bytes", a repository naming (`name == repo.name`) over a
  path naming, then the smaller name. The result for `a_13` is what it is today, by rule.

Shown: `RowView` gains `prefix: String`, computed in Rust from `(repo, org, name)`:
`org/` when the name is the repository's own name, else `<org>/<repo name>/` (`comfy/loras/`,
`lmstudio-community/Qwen3-VL-8B-Instruct-GGUF/`, `stabilityai/stable-diffusion-xl-base-1.0/`).
`table.js`, `cards.js`, `inspector.js::plugLabel` and the COMPAT rows print `prefix` in `--dim`
where they print `org/` today. `org` itself stays what it is (the true org, `None` for loose),
so search text and the peer digest are untouched.

| Artifact | Today | After |
| --- | --- | --- |
| `a_6` | `Qwen/` `Qwen3-0.6B` | unchanged |
| `a_8` | `Qwen/` `model.safetensors` | `Qwen/Qwen3-TTS-12Hz-1.7B-VoiceDesign/` `model.safetensors` |
| `a_9` | `Qwen/` `speech_tokenizer/model.safetensors` | `Qwen/Qwen3-TTS-12Hz-1.7B-VoiceDesign/` `speech_tokenizer/model.safetensors` |
| `a_14` | `mistralai/` `model` | `mistralai/Ministral-3-14B-Instruct-2512/` `model.safetensors` |
| `a_16`+`a_17` | `nvidia/` `model/model` twice | `nvidia/NVIDIA-Nemotron-Nano-9B-v2-NVFP4/` `model.safetensors` once (D2) |
| `a_218` | `NVIDIA-Nemotron-3-Super-120B-A12B-GGUF` | `lmstudio-community/NVIDIA-Nemotron-3-Super-120B-A12B-GGUF/` `NVIDIA-Nemotron-3-Super-120B-A12B-Q4_K_M.gguf` |
| `a_210` | `GLM-4.7-Flash-Q8_0.gguf` | `lmstudio-community/GLM-4.7-Flash-GGUF/` `GLM-4.7-Flash-Q8_0.gguf` |
| `a_44` | `Wan2_1-I2V-14B-480p_fp8_e4m3fn_scaled_KJ.safetensors` | `comfy/diffusion_models/` `Wan2_1-I2V-14B-480p_fp8_e4m3fn_scaled_KJ.safetensors` |

Rejected: widening `org` to carry the repository reference. It is one field with three readers
(display, search text, the `S` toast) and it would make the digest lie. Rejected: rendering the
prefix rule in JavaScript from `repo`; the rule is three branches and belongs in a Rust function
with a test. Rejected: stripping extensions from names. `flux2-vae.safetensor` and
`flux2-vae.safetensors` are two files on this machine (`a_189`, `a_190`).

Cost: the set label and the loose naming are stored: bump, Wave 2. The `prefix` field is
read-time and rides in the same wave because the table reads both.

### D4. The pull command is the file's, and the git line appears only where it works

`pull_commands(state, repo, artifact: Option<&SearchRow>)` returns:

- `hf`: for a single-file artifact, `HF_ENDPOINT=<url> hf download <reference> <rel_path>`; for
  a shard set, `HF_ENDPOINT=<url> hf download <reference> --include "<stem>-*-of-<total>.<ext>"
  --include "<stem>.<ext>.index.json"`; on the repo page (no artifact), the repository command as
  today. `--repo-type` as today. The spelling is the one `api/loadouts.rs::hf_script` already
  writes, moved into `pull_commands` so there is one writer.
- `git`: `Some(...)` only when the repository's kind is clonable (`is_clonable`), else `None`.
- `path`: unchanged.

`Y` copies `hf`, and the toast says `copied: HF_ENDPOINT=... hf download ...`. The repo page's
PULL box shows the git block only when `git` is present, and its COPY copies the command that
is shown first, which is `hf` whenever git is absent. The generated card's "Pull it" prints the
same lines. The row inside a shard set is the set; the loadout's per-item export uses the same
function.

Rejected: keeping git as the copied command with a fallback. A command that fails for 100% of a
real cache is not a default. Rejected: `--local-dir` in the command. Where the user wants the
file is theirs to say; the hint belongs in the doc string beside the box, not in the clipboard.

Cost: none stored. Wave 1, first.

### D5. The brand table is anchors and keys; the label is the key

`BRAND_TABLE` becomes `(anchor, key)`; the label column is deleted; a brand row's `label` is
its key, in the rail's own lowercase: `qwen`, `gemma`, `lfm`, `nemotron`. The tooltip
(`qwen (qwen3, qwen35, ...)`) stays and is the one place that says a row is a union. `("diffusiongemma", "gemma")` is added. `a_brand_key_maps_to_exactly_one_label` is deleted with
the column it tested.

This corrects the previous plan's Decision 1, which kept uppercase labels. The rail then has
one vocabulary: every row is a family name in lowercase mono, sorted by count, and the only
difference between a brand row and a base row is that the former has a tooltip listing members.
That is what "group by branding" asked for, and it removes both of the owner's label complaints
without adding a convention.

Rejected: "contains" matching (`diffusiongemma` contains `gemma`). An explicit anchor is one
line and cannot surprise; the previous plan's reason for anchors stands. Rejected: a
`brand:` grammar key, unchanged from the previous plan.

Cost: none stored. Wave 3.

### D6. The base label is kept only when it spells the key

At inference (`infer/base.rs`), the label recorded beside a base is kept when
`Base::new(label) == key` and dropped otherwise, so the row falls back to the key. `nemotron_h`,
`qwen3_5_moe`, `diffusion_gemma`, `muse-glimmer` stay; `qwen3vl_merger` becomes `qwen3vl`,
`StableDiffusionXLPipeline` becomes `sdxl`, `sd_v1-5` becomes `sd15`. The BASE column, the
inspector's `base` fact, the rail label (`tally.rs` observes the same stored label) and the
brand tooltip's members then agree, and what a row prints is always something `base:` accepts
up to underscores and dashes.

Rejected: a hand-written display table per base value (deferred twice already; this rule needs
none). Rejected: applying the rule at read time only. The rail label is tallied inside the
search index from the stored label, so it has to be right at write time.

Cost: bump (stored label). Wave 2.

### D7. Precision is what the bytes are, and quantisation is named when a document names it

`TensorSummary::dtypes` becomes elements per dtype (`BTreeMap<String, u64>`, from shapes, which
both sniffers already read) instead of tensors per dtype. `precision()` takes the dtype with
the most elements, excluding only true index dtypes (`I32`, `I64`, `BOOL`), ties to the narrower
float. On the table in F13 every fp8 file reads `fp8`; the NVFP4 and bnb files read `int8`,
which is their storage. Then:

- A GGUF `general.file_type` name goes through `Precision::from_dtype` when it names a float
  (`F16` -> `fp16`, `BF16` -> `bf16`, `F32` -> `fp32`) and stays as the quant name otherwise.
  The `f16` bucket disappears into `fp16`; `mmproj-model-f16.gguf`, which has no file type,
  reads `fp16` from its elements.
- When the nearest config carries a quantisation block, its scheme names the precision. Read
  from the four such repos on this machine, the scheme is the first present of
  `quantization_config.quant_algo` (`NVFP4`, modelopt), the leading token of
  `quantization_config.format` (`nvfp4-pack-quantized`, compressed-tensors),
  `quantization_config.bnb_4bit_quant_type` (`nf4`, bitsandbytes), or
  `hf_quant_config.json`'s `quantization.quant_algo` (`NVFP4`), lowercased. `quant_method`
  names the producer (`compressed-tensors`, `modelopt`, `bitsandbytes`) and is never the label.
  `Qwen3-8B-NVFP4` reads `nvfp4`, `NVIDIA-Nemotron-Nano-9B-v2-NVFP4` reads `nvfp4`,
  `qwen3-14b-unsloth-bnb-4bit` reads `nf4`. The config is already the nearest one
  `InferenceContext::config_for` resolves; `hf_quant_config.json` joins `is_model_config`.
- When the elements say an integer container and no document names a scheme, a quant token in
  the file name (`nvfp4`, `fp4`, `int4`, `int8`, `awq`, `gptq`, `bnb`) names it at
  `FILENAME_HINT` weight, which is how `z_image_turbo_nvfp4.safetensors` reads `nvfp4`. A float
  answer is never overridden by a file name.

Rejected: bytes per dtype. GGUF quant block sizes would need a table; element counts need only
shapes, and they order the same way. Rejected: leaving `int8` for NVFP4 as good enough. It is
wrong by a factor of two, and the config that says so is already open.

Cost: bump (stored tensor summary and precision). Wave 2.

### D8. Compat: an adapter names its hosts, and a projector's host is its sibling

`Role::hosts_of(self) -> &'static [Role]` replaces the flat `HOSTS`: `Lora -> [Ckpt, Llm]`,
`Ctrl -> [Ckpt]`, `Vae -> [Ckpt]`, `Upsc -> [Ckpt]`, `Emb -> [Ckpt, Llm]`, `Proj -> [Llm]`.
`is_host` is "named by any adapter". `compatible()` reads it, and `CompatTarget::allowed_roles`
already derives from `compatible()` so the grammar's `compat:` follows for free. Effect on this
catalog: an LLM's PLUGS INTO lists LoRAs, tokenizers and its projector; a checkpoint's still
lists upscalers, because a 4x upscaler after any diffusion model is a true claim.

`CompatSubject` gains `repo: String`. For `Proj` the rule is: same `repo` as the host, base
ignored. `api/compat.rs::compat_for` already builds subjects from rows that carry `repo`;
`query.rs::compat_filter` adds, for a target whose allowed roles include `Proj`, the disjunct
`(role:proj AND repo:<target repo>)` beside the base-matched roles, and for a `Proj` target
replaces the base clause with the repo term. `a_221` then lists exactly
`Qwen3-VL-8B-Instruct-Q8_0.gguf`; the gemma-4 projectors with no base list their hosts for the
first time.

This corrects the previous plan's deferral of sibling pairing. The deferral reasoned from the
scan, which commits one unit at a time; compat runs at query time over rows that carry `repo`,
so no revision-level assembly is needed.

Rejected: exact-base matching for projectors. It fixes Qwen3 versus Qwen3-VL and still pairs a
4B projector with a 12B host of the same family, and it leaves a projector with no base
unpaired. Rejected: a modality field. It is a new stored concept for one rule that a role table
expresses already.

Cost: none stored. Wave 3.

### D9. Triage is one predicate, one number, and a reason

`ArtifactRecord` gains `incomplete: bool` (from `GroupFacts::incomplete`), so the record alone
decides `needs_triage(threshold) || incomplete`, in one function every reader calls: the entry
state at write, `SummaryPart::of` for the summary, and `triage_queue`, which drops its second
filter and keeps only `!triage_cleared`. The seeded shelf becomes `state:triage`, and the seed
updates a builtin shelf's query when it differs from the seed. `TriageItem` gains
`reason: String`: `role unclassified`, `role <r> at 0.51, below 0.85`, or `3 of 10 files
present`. The queue view prints it.

On this catalog the unified predicate answers 22 today (18 unclassified, 3 low-confidence
`vae`, 1 incomplete set), and 22 after Wave 2 as well: D2 removes the placeholder set and D11
sends the DINOv3 backbone to the queue.

Rejected: a new `INCOMPLETE` state. The spec's TRIAGE means "needs a person"; a partial download
does, and the reason says which kind.

Cost: bump for the stored flag (Wave 2); the read-side unification and the shelf are Wave 4.

### D10. A duplicate is a second inode

`ArtifactRecord` gains `copies: u32`, the number of distinct inodes among its present entries,
maintained where `path_count` is (`DerivedShift`), read from the stamp already on each entry.
`SummaryPart::of` computes `reclaimable = bytes * copies.saturating_sub(1)` and
`duplicates = copies > 1`; the search index's `dupe` term and the FILES tab's `deduped` mark
follow `copies`. `path_count` keeps meaning paths. On this catalog: `2 duplicate blobs`,
`1.9 GB reclaimable`.

Cost: bump. Wave 2.

### D11. `llm` from a config needs a language-model architecture

`causal_language_model` drops the `model_type && num_hidden_layers` fallback; the config rule
fires on `architectures` naming `ForCausalLM`, `ForConditionalGeneration`, `LMHeadModel` or
`ForSeq2SeqLM`, or on `model_type` in the GGUF-derived architecture list the tensor rules
already use. `a_199` (DINOv3) leaves `llm` for triage, which is where an unrecognised vision
backbone belongs. Every LLM in this catalog that reads `llm` from a config names one of those
architectures (Nemotron `NemotronHForCausalLM`, Ministral `Mistral3ForConditionalGeneration`,
gpt-oss `GptOssForCausalLM`, Qwen3 `Qwen3ForCausalLM`).

Cost: bump (a stored role changes). Wave 2.

### D12. Paths shown to people are the paths people walked

At read time (`catalog/read.rs`, where the row is assembled), an hf-cache entry's display path
is `<root>/snapshots/<revision sha>/<rel_path>`; the blob path stays on the entry and PATHS ON
DISK prints it as `<snapshot path> -> blobs/<sha prefix>` with the `symlink` mark it has now.
The repo header's meta line and the inspector path follow the row.

Cost: none stored. Wave 4.

### D13. Small things, decided

- `any` sorts last in BASE ARCHITECTURE whatever its count, tooltip `any (works with every
  architecture)`; the bucket, its query and its count are unchanged. Wave 3.
- The `S` key and its footer hint are removed; the key sheet and `ui-spec.md` drop the line.
  A placebo is worse than an absent key. Wave 1.
- The results header prints the sort the server applied: the response carries `sort` and `dir`,
  and the default reads `SORT NAME ↑`. The name sort key is lowercased in the index. Wave 4.
- A finished job's row reads `done` in PHASE, its `subject` names the sources scanned and the
  files walked, and BYTES/RATE print `—` unless the job hashed. Wave 4.
- The SCANS `BLOBS` column and the `blobs` field are deleted. Wave 4.
- The seeded shelf reads `everything on <this node's name>` = `node:<name>`, seeded with the
  configured node name. Wave 4.
- The empty state's example is the first builtin shelf whose count is non-zero, else `role:lora`.
  Wave 4.
- `ui-spec.md`'s `LOCAL` gloss becomes "held on this node and served on the Hub face; SERVED
  marks bytes the mirror fetched". Docs only, Wave 4.

## What is not being built

- **A `brand:` or `family:` grammar key**, unchanged from the previous plan.
- **Modality as a stored field.** D8's role table says what a modality field would say, for the
  one rule that needs it.
- **A display table of human labels per base value.** D6 makes the stored label trustworthy;
  a table would be a second source of the same string.
- **Serving a loose repository under a name that also exists upstream by any rule other than
  D1.** Once the index answers first there is nothing to decide.
- **Reclassifying `text_encoders/` files or the speech tokenizer.** See "Checked and left alone".
- **A streaming performance pass.** The slow transfer in F3 was an internet download; the local
  path served at 382 MB/s on a debug build over loopback.
- **A `hoard doctor` warning for `public_url = http://127.0.0.1:7099`.** Worth a line in the
  handoff; not a wave.

## Waves

Four waves, each landing on `master` as one squashed commit through the loop in `CLAUDE.md`.
The one `FORMAT_VERSION` bump is Wave 2; nothing else touches a stored record.

### Wave 1: `pull-command` (no bump)

Ships D4 and the `S` removal. The owner's stated priority is usability, this is the largest lie
in the product, and it needs no rebuild.

- `crates/hoard-server/src/api/common.rs`: `PullCommands { git: Option<String>, hf: String,
  path: Option<String> }`; `pull_commands(state, repo, kind, artifact: Option<&SearchRow>)`;
  the shard-set spelling from `shard_count > 1` and the anchor's `rel_path` parsed with
  `shard::parse_shard_name`. `api/loadouts.rs::hf_script` calls it.
- `crates/hoard-server/src/api/artifacts.rs::pull_command`, `api/repos.rs` (the repo page's
  `pull`, and the generated card's "Pull it").
- `web/js/app.js::yank` copies `hf`; `detail.js::rightRail` renders the git block only when
  present and COPY copies the first shown; `keys.js` and `app.js` drop `S`; `web/js/keys.js`
  key sheet.
- Tests: `pull_commands` for a single file, a shard set (both `--include` globs, in that
  order), a dataset (`--repo-type`), a clonable repo (git present) and an hf-cache repo (git
  absent); the loadout export uses the same spelling. `api/tests.rs`: the artifact route's `hf`
  names the file.
- Docs: `docs/api/hoard-api.md` pull-command paragraph; `docs/design/ui-spec.md` PULL box and
  keyboard map.
- Release asset test, per `CLAUDE.md`.

Acceptance on this cache: `Y` on `a_86` copies
`HF_ENDPOINT=http://127.0.0.1:7099 hf download comfy/loras Flux_2-Turbo-LoRA_comfyui.safetensors`;
on `a_14` it copies the two-`--include` form; no page shows `git clone`.

### Wave 2: `catalog-truth` (the bump, 11 -> 12)

Ships D1, D2, D3 (stored half and `prefix`), D6, D7, D9 (the flag), D10, D11. Everything here
changes what a record holds, so it lands together and bumps once. Wave 1's command becomes true
for loose sources the moment this lands (D1).

- `crates/hoard-index/src/catalog/scan/revisions.rs` (D1), `discover/loose.rs` (the sha is
  computed where the revision is built).
- `catalog/scan/plan.rs` (D2: `creates_artifact`, `unit_roles`), `catalog/naming.rs` (D3),
  `catalog/scan/store.rs` (D3 `names_better`; D9 `incomplete`; D10 `copies`),
  `catalog/scan/counters.rs` (D10), `catalog/models.rs` (D9, D10), `catalog/search/schema.rs`
  (`dupe` from `copies`; lowercase name sort key for D13), `catalog/read.rs` (`deduped` from
  `copies`).
- `infer/base.rs` (D6), `infer/precision.rs` and `hoard-core/src/artifact.rs` (D7),
  `sniff/safetensors.rs` and `sniff/gguf.rs` (elements per dtype), `infer/rules.rs` (quant
  tokens; `hf_quant_config.json` as a model config), `infer/role.rs` (D11).
- `crates/hoard-server/src/api/common.rs` (`RowView::prefix`), `web/js/table.js`, `cards.js`,
  `inspector.js`, `detail.js` (prefix where `org/` was).
- `catalog::FORMAT_VERSION = 12`.
- Tests, each named for the property: a synthetic revision carries a sha and keeps its key by
  name; a zero-tensor container creates no artifact and anchors no set; a set is named
  `<stem>.<ext>` and two sets sharing a stem read ` (n files)`; a loose sharded model is named
  by its file, not its folder; a repository naming beats a path naming across revisions; a label
  that does not spell its key is dropped; precision follows elements (the F13 table as
  fixtures: F32 scales beside F8 weights read `fp8`; U8 beside F8 scales reads `int8` until a
  filename or config names `nvfp4`); a GGUF `F16` file type reads `fp16`; a config naming
  `DINOv3ViTModel` yields no `llm` evidence; two entries on one inode are one copy; the
  hf-cache fixture reports zero reclaimable bytes.
- Docs: `docs/native-catalog.md` (the record fields), `docs/api/hoard-api.md` (`prefix`,
  `copies`, the precision vocabulary), `docs/api/hub-compat.md` (a loose folder's `sha` is its
  synthetic sha), `docs/design/ui-spec.md` (NAME cell).
- Release asset test.

Acceptance on this cache, after the rebuild: `hf download comfy/loras Z-Detail-Slider.safetensors`
succeeds with an isolated `HF_HOME`; `GET /lmstudio-community/Qwen3-VL-8B-Instruct-GGUF/resolve/main/mmproj-...`
answers with a 40-hex `x-repo-commit` that `/api/v1/repos/...` also reports and `data/repos`
does not grow; `base:nemotronh,nemotronhmoe` answers 4 rows and none reads `model/model`;
`prec:fp8` counts at least 10 (the 6 today plus the four fp8 files measured in F13);
`prec:nvfp4` answers at least 4; `prec:f16` counts 0; `dupe:true` answers 2; DISK PRESSURE reads
about `1.9 GB reclaimable — 2 duplicate blobs`; `a_199` is in the triage queue.

The first run after this wave rescans the catalog. With the digest memo from `35b333a` /
`dd806db` that is a header re-sniff, not a rehash; the FORMAT 11 rebuild took 7 m 52 s on a
debug build. Whoever ships it says so in the delivery note, per the previous plan's rule.

### Wave 3: `rail-and-compat` (no bump)

Ships D5, D8 and the `any` ordering. This is the owner's original complaint and it depends on
nothing in Wave 2, but Wave 2 first means the rail is reviewed once, over true labels.

- `crates/hoard-index/src/search/brands.rs` (D5), `catalog/search/answer.rs` (label = key;
  `any` last in `order_buckets` for the base dimension), `web/js/facets.js` (tooltip for `any`).
- `crates/hoard-core/src/role.rs` (`hosts_of`), `hoard-core/src/compat.rs` (`repo` on the
  subject; the projector rule), `crates/hoard-index/src/search/context.rs` (`CompatTarget.repo`),
  `catalog/query.rs::compat_filter`, `crates/hoard-server/src/api/compat.rs`.
- Tests: the brand block in `catalog/tests/brands.rs` reads `qwen`; `diffusiongemma` joins
  `gemma`; `any` is last whatever its count; an LLM host accepts no upscaler; a checkpoint still
  does; a projector resolves against its sibling only, with or without a base; `compat:` on a
  projector target compiles to the repo term.
- Docs: `docs/api/hoard-api.md` (brand rows, compat rule), `docs/design/ui-spec.md` (the rail
  paragraph and the compatibility rule block), `docs/feature-plan.md` B5.
- Release asset test.

Acceptance on this cache: BASE ARCHITECTURE reads `qwen 16`, `zimage 13`, ... `gemma 8`,
`nemotron 4`, ... `lfm 2`, ... with `any 45` last; `a_15`'s PLUGS INTO lists no upscaler;
`a_221`'s lists `Qwen3-VL-8B-Instruct-Q8_0.gguf` alone; `a_231`'s lists
`gemma-4-26B-A4B-it-Q8_0.gguf`.

### Wave 4: `triage-and-legibility` (no bump)

Ships D9's read side, D12, and D13's remaining items.

- `crates/hoard-index/src/catalog/read.rs` (queue filter; display path; `reason`),
  `catalog/userdata.rs` (seed queries, node name), `crates/hoard-server/src/api/triage.rs`,
  `api/search.rs` (sort echoed), `api/jobs.rs` (`blobs` gone; finished row), `hoard-index`
  jobs (`subject`, `done` phase).
- `web/js/app.js` (sort label; example query), `table.js`, `jobs.js`, the triage view.
- Tests: the queue holds every `state:triage` row not cleared; an incomplete set's reason names
  its counts; the hf-cache row's display path is the snapshot path; the seeded node shelf names
  the node; a finished scan job reads `done`.
- Docs: `docs/api/hoard-api.md` (triage `reason`, sources without `blobs`, jobs), `ui-spec.md`
  (`LOCAL` gloss, results header).
- Release asset test.

Acceptance on this cache: `SAVED SHELVES (22)`, `triage queue 22`, `/api/v1/triage` 22,
`/api/v1/stats` 22, and every queue item carries a reason; the inspector on `a_16` shows `.../snapshots/8556c91.../model-00001-of-00010.safetensors`;
the empty state offers `flux workhorse`'s query.

### Sequencing

Wave 1 first: it is the owner's priority, it touches no record, and it turns the one action a
keyboard user has into a true one for the hf-cache today and for every source once Wave 2 lands.
Wave 2 second: it is the only rebuild, it makes Wave 1's command work for loose sources, and
Waves 3 and 4 read its output. Wave 3 before Wave 4 because it is the complaint the owner
raised; the two are otherwise independent and could swap.

Dependencies: Wave 2 D1 <- Wave 1 (the hf command is only useful once loose repos serve; the
order above is wall-clock, not dependency). Wave 4 D9 read side <- Wave 2 D9 flag. Wave 3 D8
reads `repo` on the row, present today. Nothing in Wave 3 or 4 needs a bump, and if a later
review finds one, it joins Wave 2 rather than landing alone.

## The one bump, stated plainly

Wave 2 bumps `catalog::FORMAT_VERSION` from 11 to 12. Under `CLAUDE.md`'s Compatibility section
the catalog directory is deleted and rebuilt from disk on the next start; nothing is converted.
The digest memo makes that a re-sniff rather than a rehash. Every other wave leaves
`FORMAT_VERSION` alone.

## Process notes

Two things this investigation did that the next session should know:

- Probing the Hub face made hoard fetch 1.2 GB from huggingface.co into `./data/repos`
  (`lmstudio-community/Qwen3-VL-8B-Instruct-GGUF`, `lmstudio-community/GLM-4.7-Flash-GGUF`,
  `microsoft/TRELLIS.2-4B` files, plus metadata for `facebook/dinov3-vitl16-pretrain-lvd1689m`).
  That is the F3 defect exercised, not a change; the directory is the mirror's evictable cache
  and git-ignored. Until D1 lands, any `hf download` against a loose repository whose name
  exists upstream will do the same.
- The previous plan's rule stands: when a request is about how something should look, the
  shape goes back to the owner in one sentence before a wave plan is written from it. The
  shapes in this document that needed that sentence were D3's NAME cell (`comfy/loras/` in
  dim before a ComfyUI file name) and D5's lowercase brand rows. Both are confirmed: the
  owner chose the dim repo/folder prefix on NAME and lowercase brand rows, as this document
  already specifies in D3 and D5. Everything else is a correctness decision.
