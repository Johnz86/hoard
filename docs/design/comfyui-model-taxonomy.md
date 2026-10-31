# ComfyUI's model folder taxonomy, and how it maps onto hoard today

Historical research; the source snapshots below are dated evidence. See
[current design status](README.md) for how this research informed the implementation.

Status: research summary, not a design proposal. Written to brief a follow-up IA review
(`docs/design/catalog-navigation-review.md` covers navigation; this covers the source taxonomy
that review's "base architecture" findings sit on top of). No application code changed.

## Method

Read directly from the ComfyUI checkout this workstation actually runs
(`/home/johnz86/GIT/ComfyUI`, commit `b0058496` / 2026-09-14): `folder_paths.py` (the folder
registry every loader node's dropdown is built from) and `nodes.py` / `comfy/sd.py` (the loader
node classes and the `CLIPType` enum). Corroborated against `docs.comfy.org`, the ComfyUI wiki,
and community docs for the GGUF extension, cited inline. Cross-checked against hoard's own
folder-hint table (`crates/hoard-index/src/infer/rules.rs`'s `FOLDER_ROLE_HINTS`) and against
the real `/home/johnz86/GIT/ComfyUI/models` tree on this machine.

## The mechanism: placement is the contract, not a hint

ComfyUI does no architecture sniffing of its own at scan time. `folder_paths.py` walks each
registered folder recursively (`recursive_search`) and lists whatever files match that folder's
extension set; a loader node's dropdown is exactly that list, nothing more. Which loader node a
user drags onto the canvas is what decides how a file gets interpreted — the file's own bytes
are never consulted until the graph actually runs, and a shape mismatch surfaces as a runtime
error at that point, not as a warning at scan time. So in ComfyUI's own model, **folder
placement is the classification** — there is no second opinion. That is the literal mechanism
behind the behavior the request described ("placement decides the usage in nodes").

Subfolders are preserved, not flattened away: `recursive_search` walks into them, and the
picker shows the file at its relative path (`anima/animaOfficial_preview2.safetensors` appears
as one dropdown entry, not just `animaOfficial_preview2.safetensors`). A user's own subfolders
by project or model family (this cache has `anima/`, `krea2/`, `zit/`, `pony/`, `Illustrious/`,
`wan2.2/` under `checkpoints/` and `loras/`) already give ComfyUI's own node UI a lightweight,
manual grouping for free — alphabetical clustering in a long dropdown. Nothing about that
grouping reaches hoard today: `org` is null for essentially all of it (Finding 2,
`catalog-navigation-review.md`), and the path is stored but not surfaced as a facet or a label.

## The folder registry, as `folder_paths.py` defines it today

| Registered key | Physical folder(s) scanned | Loader node(s) | Loader output | Standalone? |
| --- | --- | --- | --- | --- |
| `checkpoints` | `checkpoints/` | `CheckpointLoaderSimple`, `unCLIPCheckpointLoader` | `(MODEL, CLIP, VAE[, CLIP_VISION])` | Yes — a complete pipeline in one file |
| `diffusion_models` | `unet/` **and** `diffusion_models/` (same picker) | `UNETLoader` | `(MODEL,)` only | No — needs a separately loaded CLIP and VAE wired in the graph |
| `text_encoders` | `text_encoders/` **and** `clip/` (same picker) | `CLIPLoader` (one file), `DualCLIPLoader` (two files + a `type` the user picks by hand: `sdxl`, `flux`, `hidream`, `hunyuan_image`, ... 18 options) | `(CLIP,)` | No — feeds a diffusion graph, produces nothing alone |
| `clip_vision` | `clip_vision/` | `CLIPVisionLoader` | `(CLIP_VISION,)` | No — vision-conditioning input, distinct from text encoders |
| `vae` | `vae/` (`vae_approx/` merged in for fast-preview options) | `VAELoader` | `(VAE,)` | No — paired with a MODEL to decode/encode |
| `loras` | `loras/` | `LoraLoader`, `LoraLoaderModelOnly` | patches an existing `MODEL` (+ optionally `CLIP`) | No — always requires a base model already loaded |
| `controlnet` | `controlnet/` **and** `t2i_adapter/` (same picker) | `ControlNetLoader` | `(CONTROL_NET,)` | No — patches a sampling pass against a loaded MODEL |
| `upscale_models` | `upscale_models/` | `UpscaleModelLoader` | `(UPSCALE_MODEL,)` | Yes — a standalone pixel-space upscaler, no base model needed |
| `diffusers` | `diffusers/` | `DiffusersLoader` | `(MODEL, CLIP, VAE)` | Yes, but the "file" is a whole HF-diffusers directory (`["folder"]`, not file extensions) |
| `embeddings`, `hypernetworks`, `gligen`, `style_models`, `photomaker`, `latent_upscale_models` | one folder each | specialized loaders | varies | smaller categories, not detailed here |

Sources: `folder_paths.py:25-67` (the registry), `nodes.py:616-1080` (the loader classes),
`comfy/sd.py:1528` (`CLIPType`); corroborated by
[ComfyUI docs — Models](https://docs.comfy.org/development/core-concepts/models),
[ComfyUI Wiki — Folder Structure](https://comfyui-wiki.com/en/interface/files), and the
[ComfyUI-GGUF loader docs](https://www.runcomfy.com/comfyui-nodes/ComfyUI-GGUF/UnetLoaderGGUF)
(GGUF diffusion models are still commonly documented as going in `unet/`, the pre-rename name).

## Two folder pairs are literal aliases inside ComfyUI itself

`folder_names_and_paths["diffusion_models"]` scans **both** `models/unet/` and
`models/diffusion_models/` into one picker (`folder_paths.py:31`), and
`folder_names_and_paths["text_encoders"]` scans **both** `models/text_encoders/` and
`models/clip/` into one picker (`folder_paths.py:30`). `controlnet`/`t2i_adapter` is a third
such pair (`folder_paths.py:38`). These aren't three separate categories dressed up as one —
ComfyUI's own source treats each pair as a single category with two accepted folder spellings,
the older name kept for backward compatibility as the ecosystem's vocabulary shifted (`unet`
predates the "not every diffusion model is a U-Net" DiT generation; `clip` predates text
encoders like T5, UMT5, Llama and Gemma variants that aren't CLIP at all but are loaded the
same way). A design that treats `checkpoints`/`diffusion_models`/`unet`/`text_encoders`/`clip`
as five independent categories is already more fragmented than ComfyUI's own model.

## Where hoard's inferred role lines up with this — and where it doesn't

Cross-referencing this cache's real `checkpoints/`, `diffusion_models/`, and `text_encoders/`
contents (46 + 17 files) against what `/api/v1/search` reports:

| ComfyUI folder | What ComfyUI treats it as | hoard `role` observed | Agreement |
| --- | --- | --- | --- |
| `checkpoints/` | complete pipeline (`MODEL, CLIP, VAE`) | `ckpt` for 6/10, `unk` for 3/10, `llm` for 1/10 | Partial — see the two bugs below |
| `diffusion_models/` | backbone only, needs a paired CLIP + VAE | `ckpt` for 34/36, `unk` for 2/36 | Mostly — hoard's `ckpt` doesn't distinguish "complete" from "needs pairing," which `checkpoints/` vs `diffusion_models/` exists specifically to communicate |
| `text_encoders/` | feeds `CLIPLoader`/`DualCLIPLoader`, always a `CLIP` output | **split** between `ckpt` and `llm` depending on internal architecture (see below) | No — same folder, same ComfyUI purpose, two hoard categories |

Two things already reported stand confirmed by this cross-reference and are restated here only
because this taxonomy explains *why* they happen:

- `checkpoints/hidream_o1_image_dev_fp8_scaled.safetensors` gets `role: llm`. `DualCLIPLoader`'s
  own recipe list says a standard HiDream setup pairs a separate T5/Llama text encoder (in
  `text_encoders/`) with a DiT backbone (in `diffusion_models/`); this particular file is an
  all-in-one fusion of both into one `checkpoints/`-filed artifact (no stock ComfyUI node loads
  that exact fused shape — it needs a third-party sampler node), which is exactly the shape that
  trips hoard's causal-LM tensor rule on the embedded language-model component.
- The `checkpoints/anima/`, `hunyuan_3d_v2.1.safetensors` and `diffusion_models/pixal3d_*`
  files land in `unk`/triage because no tensor rule recognizes their key namespace, and hoard's
  `FOLDER_ROLE_HINTS` already maps `checkpoints/` and `diffusion_models/` to `Role::Ckpt` but at
  a confidence capped below the classification floor, so it can never rescue them.

### A new finding this taxonomy surfaces: `text_encoders/` splits inconsistently

Every file in this cache's `text_encoders/` folder is, by ComfyUI's own model, the same kind of
thing: something you hand to `CLIPLoader` or `DualCLIPLoader` to get a `CLIP`-typed graph input
for conditioning a diffusion model. None of them are meant to be run as a standalone chat model.
hoard's role inference sees them differently depending on internal architecture family:

| File | Architecture | hoard `role` |
| --- | --- | --- |
| `umt5_xxl_fp8_e4m3fn_scaled.safetensors` | UMT5 (encoder-decoder) | `ckpt` |
| `byt5_small_glyphxl_fp16.safetensors` | ByT5 (encoder-decoder) | `ckpt` |
| `gemma_3_12B_it.safetensors`, `gemma4_e4b_it_fp8_scaled.safetensors` | Gemma (decoder-only) | `llm` |
| `mistral_3_small_flux2_bf16.safetensors`, `ministral-3-3b.safetensors` | Mistral/Ministral (decoder-only) | `llm` |
| `qwen_3_4b.safetensors`, `qwen_2.5_vl_7b.safetensors`, etc. (9 files) | Qwen (decoder-only) | `llm` |

The split tracks a real, defensible technical fact — T5-family encoder-decoder architectures
don't match hoard's causal-LM tensor rule, decoder-only Gemma/Mistral/Qwen architectures do —
but it's invisible and unmotivated from where the user is standing: these are 13 files in one
folder, all playing the identical role in every workflow that uses them (drop into
`DualCLIPLoader`, pick a `type`, wire the `CLIP` output into a sampler), and hoard's rail
currently shows most of them mixed into the same `ROLE` bucket as this cache's 58 actual
`role:llm` artifacts — real standalone chat/instruct models like `Qwen/Qwen3-0.6B` or
`nvidia/NVIDIA-Nemotron-3-Nano-30B-A3B-BF16` that a user would run for text generation, not
plug into an image pipeline. Browsing `role:llm` today mixes "models you converse with" and
"components a diffusion workflow silently needs," disambiguated only by opening each one and
reading its `path`.

## Summary for the IA review

Three structural facts a follow-up design pass over hoard's categories should treat as given:

1. ComfyUI's taxonomy is folder-first and manual — the user's placement decision *is* the
   category, with two explicit alias pairs (`unet`=`diffusion_models`, `clip`=`text_encoders`)
   already built in. hoard's taxonomy is content-first and inferred, and currently does not read
   folder placement as a decisive signal (`FOLDER_HINT` confidence 0.35 against a 0.5 floor).
2. `checkpoints/` (self-contained) and `diffusion_models/` (backbone-only, needs pairing) are a
   distinction ComfyUI users make constantly when deciding whether a file is immediately usable
   — hoard's single `ckpt` role currently erases it.
3. `text_encoders/` (feeds a diffusion graph as a `CLIP`) and true standalone language models
   (`role:llm` proper) are functionally two different things ComfyUI users reach for in two
   different situations, and hoard's tensor-architecture-only classification currently merges
   them into one rail bucket by accident of which files happen to have decoder-only internals.
