# Catalog consistency review: brand rail, naming and the pull-command workflow

Historical review, resolved by the catalog UX waves. See [current design status](README.md)
before treating any finding below as an outstanding defect.

Investigation against the real running server (debug build at commit `5de7193`, catalog
`FORMAT_VERSION` 11, the real ~314 GB local cache) and its real `/api/v1` responses, plus one
browser screenshot of the rendered UI. Read-only: no code was changed to produce this report.
Ordered by how much each finding would actually mislead or block a user trying to find and
get a model, not by how the issues were originally reported.

Evidence artifacts: `/tmp/hoard-ux-review/03-nemotron-list.png` (full rail + NVIDIA NEMOTRON
artifact table + inspector, screenshotted this session), raw JSON captured from
`GET /api/v1/search` with various `base:` filters.

---

## 1. The clone/download command is never scoped to the file a user selected - it always
clones or downloads the whole upstream repo

**What's wrong.** Every path in the app that produces a "get this" command - the `Y` (yank)
keyboard shortcut on any row, and the copy button on a repo's own detail page - hands back a
`git clone <repo>` command scoped to the *repository*, never to the specific artifact, file,
shard set, or quantization the user actually selected. The `hf download` command shown on the
detail page has the same problem. For any repo that holds more than one file's worth of what
a user might want (almost every quantized-model repo, every repo with multiple precisions,
every multi-shard checkpoint), the action a user takes after finding "the artifact I want" pulls
everything in that repo, silently, with nothing in the UI saying so.

**Evidence.**
- `crates/hoard-server/src/api/common.rs:461-472`:
  ```rust
  pub fn pull_commands(state: &AppState, repo: &RepoId, path: Option<String>) -> PullCommands {
      let url = state.public_url().trim_end_matches('/');
      let reference = repo.reference();
      let repo_type = match repo.repo_type() { ... };
      PullCommands {
          git: format!("git clone {url}/{reference}"),
          hf: format!("HF_ENDPOINT={url} hf download {reference}{repo_type}"),
          path,
      }
  }
  ```
  Both `git` and `hf` are built from `reference` (the *repository* reference) alone. `path` is
  accepted as a parameter and stored on the returned struct, but never appears inside either
  command string - there is no `--include`, no file argument, nothing that would scope the
  download to one file.
- The artifact-specific endpoint does compute a real path:
  `crates/hoard-server/src/api/artifacts.rs:194-203` builds `path = row.abs_path.clone().or(Some(row.path.clone()))`
  and passes it into `pull_commands`, so the JSON response for a specific artifact genuinely
  carries the right file path in its `path` field.
- But nothing consumes it. `web/js/app.js:452-459`:
  ```js
  async function yank(row) {
      if (!row || !row.artifact_id) return;
      try {
          const command = await api.pullCommand(row.artifact_id);
          await copyCommand(command.git);
      } ...
  }
  ```
  only ever copies `command.git` - the whole-repo clone command - never `command.hf` and
  never anything built from `command.path`.
- `web/js/detail.js:283-293` renders both `pull.git` and `pull.hf` as read-only text on the
  repo detail page, but the one copy button there (`onClick: () => app.copyCommand(pull.git || "")`)
  also only ever copies the repo-level `git` command. `pull.path` is not rendered anywhere in
  `web/js/*` that a grep for `.path` found - the computed value is dead data from the
  frontend's point of view.

**Root cause.** `crates/hoard-server/src/api/common.rs::pull_commands` (the one function that
builds every command string in the app) never incorporates its own `path` argument into either
command it returns.

**Impact on the actual workflow the owner asked about.** A user browses to
`NVIDIA-Nemotron-3-Nano-4B-Q8_0.gguf` (4.2 GB), decides that's the one they want, presses `Y`.
If that file's upstream repo also ships other quantizations or precisions (an extremely common
HF pattern - this exact catalog has several repos structured that way, see Finding 2), the
copied command clones or downloads the entire repository: every quant, every shard, every
precision variant, with no size warning and no indication that a scoped alternative exists.
The tool's implied promise - find an artifact, get that artifact - does not hold for any
multi-file repository.

---

## 2. Two different real files, one local and usable, one a tiny triage stub, render as the
identical name "nvidia/model/model"

**What's wrong.** In the NVIDIA NEMOTRON brand group, two rows in the artifact table both
display the name `nvidia/model/model`. One is a real 7.8 GB local checkpoint; the other is a
320-byte file still sitting in the triage queue. Nothing in the visible name distinguishes
them - a user has to notice and correctly interpret the SIZE and STATE columns to tell a
usable 7.8 GB model apart from a 320-byte stub.

**Evidence**, from `GET /api/v1/search?q=base:nemotronh,nemotronhmoe` (and confirmed
identically in the rendered UI, `/tmp/hoard-ux-review/03-nemotron-list.png`):

```
artifact_id: a_16   name: "model/model"   repo: models/nvidia/NVIDIA-Nemotron-Nano-9B-v2-NVFP4
             size: 7,835,224,960 (7.8 GB)   state: local   blob_sha256: cedcf7e8...
artifact_id: a_17   name: "model/model"   repo: models/nvidia/NVIDIA-Nemotron-Nano-9B-v2-NVFP4
             size: 320 (320 B)              state: triage  blob_sha256: 60d95b10...
```

Same repo, same org, same displayed name, different real blobs, different real sizes,
different states - and a real repo name that is genuinely informative
(`NVIDIA-Nemotron-Nano-9B-v2-NVFP4`) never appears anywhere in the row's own `name` field.

A catalog-wide check (`GET /api/v1/search?q=&limit=500`, 233 artifacts total) found exactly
one `(repo, name)` collision today - this pair - so the bug is not currently widespread, but
the mechanism that produces it is generic and will recur on any other repo shaped the same
way (see root cause below). A softer version of the same defect - an uninformative but at
least non-colliding `name: "model"` - already exists on a second, unrelated repo
(`models/mistralai/Ministral-3-14B-Instruct-2512`), showing this naming path is a real,
recurring pattern in this catalog, not a one-off.

**Root cause, precisely.**
- `crates/hoard-index/src/shard.rs::named_sets` (lines 196-234) groups files into shard sets
  by `SetKey { directory, stem, extension, total }` (`shard.rs:187-193`). The NVFP4 repo
  ships two file-naming families that both parse to `stem == "model"` in the top-level
  directory - most likely a 2-shard set (e.g. `model-00001-of-00002.safetensors`, bf16) and
  an 8-shard set (e.g. an NVFP4-quantized `model-00001-of-00008.safetensors`) - which differ
  only in `total`, so `named_sets` correctly treats them as two distinct `ShardSet`s, each
  with `stem: "model"`.
- `crates/hoard-index/src/catalog/naming.rs::shard_label` (lines 119-127) labels a
  multi-file set by `file_name(&set.directory)` when the shards sit in a named subdirectory,
  falling back to `set.stem` when they sit at the repo's top level (`directory` is empty).
  Both sets here fall back to `stem`, so both compute `shard_label == "model"`.
- `crates/hoard-index/src/catalog/scan/plan.rs::set_labels` (lines 272-289) collects every
  set's label and every set's stem for one revision, then calls
  `disambiguate_labels(&mut labels, &stems)` to break ties.
- `crates/hoard-index/src/catalog/naming.rs::disambiguate_labels` (lines 129-144):
  ```rust
  pub(super) fn disambiguate_labels(labels: &mut [String], stems: &[String]) {
      let mut counts: HashMap<&str, usize> = HashMap::new();
      for label in labels.iter() { *counts.entry(label.as_str()).or_default() += 1; }
      let shared: Vec<bool> = labels.iter().map(|label| counts...> 1).collect();
      for ((label, stem), shared) in labels.iter_mut().zip(stems).zip(shared) {
          if shared { *label = format!("{label}/{stem}"); }
      }
  }
  ```
  This correctly *detects* the collision (both labels are `"model"`), but its disambiguation
  strategy - append the set's own stem - assumes the stem is what differs between colliding
  sets. Here it is not: both sets' stems are *also* `"model"` (that is precisely why their
  labels collided in the first place), so the "disambiguated" result is `"model/model"` for
  both rows - a no-op that changes the string without distinguishing anything, and actively
  makes it look like a formatting artifact rather than two different real files.

**Impact.** A user filtering to this brand cannot tell these two rows apart from their name
alone. The `Y` yank shortcut (see Finding 1) would give the identical `git clone` command for
either row regardless, which happens to make the *download* harmless to get wrong - but a user
trying to reason about "which of these two do I have locally and usable" has no way to do so
from the name, and the 320-byte triage stub is presented as a full peer row at the same visual
weight as the real model, which reads as the catalog being broken or duplicated rather than as
"one file still needs classifying."

---

## 3. The base rail mixes two label conventions with no explanation, so a related value can
sit far from its group

**What's wrong.** Every brand-grouped rail row (`QWEN`, `GEMMA`, `LFM`, `NVIDIA NEMOTRON`)
renders in bare uppercase. Every standalone, ungrouped row renders lowercase in its raw
normalized storage spelling (`diffusion_gemma`, `clip_text_model`, `muse-glimmer`,
`dinov3_vit`, `gpt_oss`, `mistral3`, `sdxl`, `zimage`, `flux`, `krea2`, `wan`, `deepseek2`,
`pixtral`). Nothing in the UI - no icon, divider, header, or tooltip - explains that these are
two different kinds of row (a merged group vs. a single literal base value); a user has to
infer it purely from casing.

The owner's specific example is real and reproducible: `diffusion_gemma` is a genuine, distinct
stored base value (`GET /api/v1/search?q=&limit=0` -> `base` facet includes
`{"value": "diffusiongemma", "label": "diffusion_gemma", "count": 1}`), sitting several rows
below `GEMMA` in the rail's count-sorted order, with no visual or structural link between them
despite the obvious name relationship. It does not merge into `GEMMA` because `crates/hoard-index/src/search/brands.rs::BRAND_TABLE`'s membership rule is an exact-prefix match against `gemma3`/`gemma4`
only (`brand_of`, same file), and `diffusiongemma` does not start with either anchor.

**Root cause.** `crates/hoard-index/src/search/brands.rs::BRAND_TABLE` is a short, hand-maintained
allowlist of anchor->label pairs; any base value not in it (however closely related by name)
renders as its own raw row, in the raw casing/underscoring the base-normalization step already
produced, which is visually a completely different style than the curated uppercase brand
labels sitting above it in the same list.

**Impact.** A user scanning the rail for "everything Gemma-shaped" sees `GEMMA (7)` near the
top and has no reason to also look nine rows down for `diffusion_gemma (1)` - the UI presents
them as unrelated categories despite one being a variant of the other.

---

## 4. The brand table's naming rule is inconsistent - one brand names the company, the other
three do not

**What's wrong**, confirmed on the rendered rail (`/tmp/hoard-ux-review/03-nemotron-list.png`):
of the four grouped brand rows, three are bare model-family names (`QWEN`, `GEMMA`, `LFM`) and
one is `NVIDIA NEMOTRON` - the only row on the entire ~18-row rail that carries a company name
at all.

**Root cause.** `crates/hoard-index/src/search/brands.rs::BRAND_TABLE`:
```rust
pub const BRAND_TABLE: &[(&str, &str, &str)] = &[
    ("qwen3", "qwen", "QWEN"),
    ("gemma3", "gemma", "GEMMA"),
    ("gemma4", "gemma", "GEMMA"),
    ("lfm2", "lfm", "LFM"),
    ("nemotronh", "nemotron", "NVIDIA NEMOTRON"),
];
```
The third element of each tuple is a free-text label with no naming rule behind it - it was
chosen per-entry rather than derived from a consistent policy (e.g. "always the model-family
name alone," or "the company name only when the family name alone would be ambiguous with
something else already on the rail" - nemotron does not appear to meet even that bar, since
nothing else on this rail is named `nemotron`).

**Impact.** A user cannot form a reliable expectation of what a brand row will be called;
each one has to be learned individually rather than inferred from a pattern, and the one
inconsistent entry reads as arbitrary rather than intentional.

---

## 5. Within one brand group, whether an org prefix is shown depends on which source found
the file, not on whether the org is actually known

**What's wrong.** Of NVIDIA NEMOTRON's 5 rows, 3 (from the HF cache) render with an
`nvidia/` prefix and 2 (from a loose LM Studio folder) render with none at all -
`NVIDIA-Nemotron-3-Nano-4B-Q8_0.gguf` and `NVIDIA-Nemotron-3-Super-120B-A12B-GGUF` - even
though the org is unambiguously spelled out in both filenames.

**Root cause.** `crates/hoard-index/src/catalog/naming.rs::artifact_name` (lines 148-168):
```rust
let org = match naming.kind {
    SourceKind::Loose | SourceKind::Auto => None,
    SourceKind::HfCache | SourceKind::Git => naming.repo.org().map(str::to_owned),
};
```
`org` is decided purely by which kind of source the file was discovered under, never by
inspecting the filename or any other signal - a defensible backend rule (a loose folder
carries no Hub org metadata to read), but with no UI signal that explains why the same brand's
rows are inconsistently formatted.

**Impact.** Within a single brand-filtered list, 3-of-5 rows look like `org/name` and 2-of-5
look like a bare filename, which reads as inconsistent curation rather than as "these came
from a different kind of source," since the UI never says so.

---

## 6. "PLUGS INTO" compat suggestions cross modality boundaries via the `base: any` wildcard

**What's wrong.** Selecting the NVIDIA-Nemotron-3-Nano-30B text LLM checkpoint
(`artifact_id: a_15`) and opening its inspector shows a "PLUGS INTO" panel listing items like
`ltx-2-spatial-upscaler-x2-1.0.safetensors` and `4xNomos8kDAT.pth` - image/video upscalers,
nothing to do with a text model. Confirmed directly via
`GET /api/v1/artifacts/a_15/compat`, which returns several `role: upsc` rows all from
`models/comfy/latent_upscale_models`.

**Root cause.** These upscaler artifacts are stored with `base: any` (the single largest
bucket on the whole rail, 45 of 233 artifacts), and hoard's own documented compatibility rule
(`docs/feature-plan.md` B5: "adapters ... resolve against hosts ... with the same base or base
`any`") treats `any` as an unconditional match against every host, with nothing checking that
the adapter and host are even the same kind of model (text vs. image/video).

**Impact.** The panel meant to help a user find the pieces that complete a model instead
regularly surfaces irrelevant, un-actionable suggestions for any host paired with a
`base:any` adapter. A user who sees an LLM "plug into" an image upscaler once has good reason
to stop trusting the panel altogether.

---

## 7. Minor: the artifact count under NVIDIA NEMOTRON does not match what was originally
reported, with no visible explanation of why

The owner's report cited 6 artifacts; both the live facet count and the direct query return 5
(`GET /api/v1/search?q=base:nemotronh,nemotronhmoe` -> `"total": 5`). This may simply mean a
rescan or a state change happened between the original observation and this investigation -
the catalog's own job log shows a scan completing recently - but there is no user-visible
"last changed" note anywhere a facet count appears, so there is no way to tell from the UI
itself whether a number like this moved, or why. Noted as a data point, not as a bug in itself.

---

## What was checked and found fine

- No cross-repo `(org, name)` collision exists anywhere in the current 233-artifact catalog -
  the collapse in Finding 2 is currently confined to within one repository, which is the least
  dangerous shape this bug could take (a wrong pick still downloads from the right repo).
- The artifact-level pull-command endpoint does compute the correct specific file path
  (`crates/hoard-server/src/api/artifacts.rs:200`); the defect in Finding 1 is that this
  correct data is never surfaced into an actual command or the UI, not that it is computed
  wrong.
- `role:needs-pairing`/brand-membership/base-grouping mechanics landed earlier this session
  were not re-litigated here except where they directly produced a rail-legibility problem
  (Findings 3-4); this report does not revisit whether the flat-brand-rail design itself was
  the right call, only whether its current data is consistent and legible.
