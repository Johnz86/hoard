The use case
A practitioner with weights scattered across four disks and nine tools. Downloads landed wherever the fetching tool put them: ~/comfy/models/loras, /mnt/vault/hf, a scratch drive, a NAS. Filenames are the only metadata, so sd_xl_refiner_FINAL_v2.safetensors is unidentifiable without loading it. The same file exists three times because three front-ends each fetched it.

The variety is the hard part. A checkpoint, a LoRA, a ControlNet, an upscaler, a VAE, an embedding, a GGUF language model and a 412 GB dataset are all "models" and none of them share a schema, a size range, or a question you'd ask about them. The one question that matters, which of these actually load together, no existing tool answers.

Then the network layer: the workstation holds the bytes, the laptop needs them, and copying 40 GB again is the current answer.

Design decisions
Meaning lives in the index, not the path. Four primitives, repo, revision, artifact, role, and role is inferred from tensor keys, shapes and rank, not the folder. Nothing is moved or renamed; the filesystem stays authoritative. This is what makes one file appear on twenty shelves.

Search is the primary interface, and it's a grammar, not a filter panel. role:lora base:sdxl size:<400mb compat:sdxl-base-1.0, with a visible parsed-token strip so the query is legible rather than magic. The same grammar defines shelves, so a saved view is just a query. Facet counts on the left are computed excluding their own dimension, so clicking never dead-ends into zero.

compat: is the marquee feature and it's directional. Adapters resolve against host checkpoints, never against each other, a LoRA listing other LoRAs as compatible would give away that it was only string-matching a base field.

Density over comfort. 28 px rows, nine scannable columns, keyboard-first (j/k, enter, space, L, S, /). Cards view exists only for LoRAs, where visual identity matters. The inspector rail means arrow-keying down a list gives you facts without navigation.

The README is treated as a document, not a blob. Typeset properly on a paper-panel with YAML frontmatter shown separately, served from the clone at a specific revision, and when there's no upstream card, the page is generated from the index record instead of showing an empty state. That's the artifact people actually read.

Loadouts as the real unit of work. Checkpoint + LoRAs at weights + ControlNet + VAE + upscaler, pinned by commit, portable as a text file. Nobody uses one model.

Typography and palette
Space Grotesk and Space Mono, amber on near-black. Mono carries every measured value, sizes, shas, paths, counts, so numbers align and scan; sans carries only prose and names. The proposal deck inverts to paper-white for narrative and drops to the dark surface for product screens, which keeps the two registers distinct. One accent, used only for state and focus, so nothing decorative competes with the eight role colors.

