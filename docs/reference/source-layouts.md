# Source layout classification

Source layout records where a resource was filed, independently of the role inferred from
its contents. The implementation is [layout.rs](../../crates/hoard-index/src/layout.rs);
the API exposes this dimension as the SOURCE LAYOUT facet and `layout:` query key.

## Source kinds

| Source kind | Layout label |
| --- | --- |
| Hugging Face cache | `hub repo` |
| Git repository | `git clone` |
| Loose folder | Canonical category from the table below, or `(unplaced)` |
| Unresolved/missing kind | `(unplaced)` |

Git/cache subfolders do not become model-layout categories. A repository's internal `loras`
folder therefore does not reclassify its source as a loose LoRA tree. For loose sources,
classification uses the category repository name, not an arbitrary substring of its path.

## Canonical folder values

Matching is case-insensitive. These are the mappings Hoard supports, not a claim about every
folder or loader supported by another tool's current release.

| Folder spelling | Canonical layout |
| --- | --- |
| `checkpoints`, `stable-diffusion` | `checkpoints` |
| `unet`, `diffusion_models` | `diffusion_models` |
| `clip`, `text_encoders` | `text_encoders` |
| `clip_vision` | `clip_vision` |
| `vae`, `vae_approx` | `vae` |
| `lora`, `loras` | `loras` |
| `controlnet`, `t2i_adapter` | `controlnet` |
| `esrgan`, `upscale_models` | `upscale_models` |
| `latent_upscale_models` | `latent_upscale_models` |
| `embeddings` | `embeddings` |
| `hypernetworks` | `hypernetworks` |
| `gligen` | `gligen` |
| `style_models` | `style_models` |
| `photomaker` | `photomaker` |
| `diffusers` | `diffusers` |
| `model_patches` | `model_patches` |

Typed aliases resolve to the same canonical query value, so `layout:unet` selects
`diffusion_models`. Unknown spellings do not silently become a valid layout. Tree discovery
uses a separate marker classification: two distinctive neighboring folders identify a tool
models root. Not every recognized category is distinctive enough to serve as that marker.

Recognized ComfyUI and A1111 trees keep one repository per top-level category, including
categories whose models all live in subfolders. Removing the last file directly inside a
category does not reinterpret that category as a publisher or rename its repositories.
Nested paths are preserved inside the category repository.

Layout is useful placement evidence, not a replacement for tensor/header inference. Keep
role, source kind, model family and layout separate in storage, search and presentation.
