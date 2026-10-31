# Hoard JSON API (`/api/v1`)

Consumed by the web UI, the CLI and peer nodes. Namespaced under `/api/v1` so it never
collides with the Hub-compatible `/api/{models,datasets,spaces}` routes. Responses are
JSON; errors are `{ "error": { "code": "<PascalCase>", "message": "<text>" } }` with a
matching status. Mutating routes require `Authorization: Bearer <HOARD_NODE_TOKEN>` when
that variable is set; read routes are open on the LAN.

Field vocabularies are the grammar spellings: `role` in `ckpt lora ctrl upsc vae emb llm
ds proj unk`, `sub_role` in `ipa tok mot cmp bb` or null, `state` in `served local pulling
stale remote triage`, `format` in `safetensors gguf diffusers transformers pth pt bin onnx
parquet arrow jsonl csv readme config unknown`.

## Search

`GET /api/v1/search?q=<grammar>&sort=<col>&dir=asc|desc&cursor=<opaque>&limit=100`

```json
{
  "query": { "raw": "role:lora base:sdxl size:<400mb film grain",
             "tokens": [ { "key": "role", "value": "lora", "kind": "role", "raw": "role:lora" },
                         { "key": "size", "op": "<", "value": 400000000, "kind": "size", "raw": "size:<400mb" },
                         { "key": null, "value": "film grain", "kind": "text", "raw": "film grain" } ],
             "warnings": [] },
  "facets": {
    "role": [ { "value": "lora", "label": "LoRA", "count": 412, "active": true,
                "token": "role:lora", "query": "role:lora", "members": [] } ],
    "layout": [], "base": [], "format": [], "precision": [], "node": [],
    "disk_pressure": { "reclaimable_bytes": 333000000000, "duplicate_blobs": 47,
                        "label": "310.0 GB reclaimable — 47 duplicate blobs" },
    "base_total": 213, "base_shown": 27 },
  "rows": [ {
    "artifact_id": "a_01H...", "blob_sha256": "9f2c...41ab", "role": "lora", "sub_role": null,
    "role_confidence": 0.98, "org": "kohya", "prefix": "kohya/", "name": "film-grain-35mm",
    "base": "sdxl 1.0",
    "format": "safetensors", "precision": "fp16", "size": 239075328, "rank": 32, "alpha": 16,
    "params": null, "context": null, "quant": null, "revision": "main", "revision_sha": "462c3cb...",
    "node": "workstation", "layout": "loras", "state": "served", "pull_progress": null,
    "trigger": "film grain, 35mm, halation", "path": "/mnt/vault/loras/film-grain-35mm.safetensors",
    "path_count": 2, "shard_count": 1, "incomplete": false,
    "last_used": "2026-09-04T09:10:00Z" } ],
  "next_cursor": "...", "total": 212, "nodes": 3, "summary": "212 ARTIFACTS · 3 NODES",
  "sort": "name", "dir": "asc"
}
```

`sort` and `dir` are the sort the server actually applied, spelled the way the grammar spells
a column, so the results header names it rather than guessing from what a click last asked
for. A request naming neither gets back `name` and `asc`. The name sort orders on a
lower-cased copy of the name, so `ae.safetensors` precedes `Z-Detail-Slider`.

A row's `prefix` is what the UI prints in `--dim` before the name, so the repository a name
was chosen from is always on screen. It is derived in Rust from the row's qualified `repo`,
never from `org`: it is `org/` when the name is the repository's own name, and
`org/repository/` when the name is a path inside the repository or the artifact's own file
name. A loose artifact carries no `org` at all, so its prefix comes from the folder path its
repository is (`comfy/diffusion_models/`). An orgless repository naming its one artifact
prints nothing. `CompatRowView` carries the same field, built by the same function.

`facets.layout` is the folder discovery filed the entry's repository under, which is the
placement decision the tree already carries rather than anything inference concluded: a
ComfyUI or A1111 folder folded onto its canonical spelling (`unet/` reads `diffusion_models`,
`clip/` reads `text_encoders`, `t2i_adapter/` reads `controlnet`, `vae_approx/` reads `vae`,
`esrgan/` reads `upscale_models`), or `hub repo` for an hf-cache entry, `git clone` for a git
one, and `(unplaced)` for a folder no tool registers. `layout:` filters on it, folding a
folder spelling onto its canonical layout the way `role:` folds its aliases (`layout:unet`
matches `diffusion_models`), and warns on a value naming neither a canonical layout nor a
folder spelling.

A base is the finest architecture the header, the metadata or the path proves, and a lineage
is stored under its own key rather than as a suffix of the architecture it refines: `pony`,
`illustrious`, `noobai`, labelled `Pony`, `Illustrious` and `NoobAI`.
`hoard_core::Base::LINEAGES` is the one table that says which architecture each lineage runs
on. `base:` filters one way, from the architecture down: `base:sdxl` finds every lineage of
SDXL and `base:pony` finds the Pony ones alone, never the files known only as `sdxl`.
`compat:` is the two-way test, because resolving an adapter against a host of the bare
architecture is a compatibility question and not a question about which family a file belongs
to: a Pony adapter fits a Pony host or a plain SDXL one, and never an Illustrious one.

`facets.base` contains at most 48 model family rows. Exact indexed base values are grouped
before that limit, so a displayed family includes even its less common variants. Families
are ranked by the sum of their exact-term populations, then each displayed row is counted
with the same union query its click opens. A family outside the limit is not shown; a query
naming one of its exact bases still answers. The `any` bucket is active only when a
positive `base:` token holds the value `any`, and the value `any` marks no other bucket, so
`base:any,flux` marks both and `base:flux` does not mark `any`, although compatibility still
reads a stored `any` as matching every base.

`facets.base_total` is how many distinct bases the result set holds by exact term, counted
before grouping and the limit, so a rail standing for
fewer bases than `base_total` is hiding the rest and can say so. It answers only for `base`;
no other dimension is capped.

`facets.base` is one flat list of lowercase family names without version numbers. Known
aliases combine related roots such as `qwen` and `qwenimage`, `zimage` and `zimageturbo`, and
all `flux`, `ltx`, and `wan` variants. Other bases use the letters before their first digit.
Every row's `members` lists its exact indexed bases. Its `token` and `query` select those
members as a union, so an artifact matching overlapping base prefixes counts once. A row is
active only when every member is selected. The `any` row sorts last.

`facets.base_shown` is how many exact bases the displayed family rows stand for. Comparing it
with `base_total` shows whether the rail hides any families.

Role facet rows use readable labels such as `Checkpoint`, `ControlNet`, and `Dataset`, while
their `value` and `token` retain the stored codes (`ckpt`, `ctrl`, `ds`). Search accepts both
spellings, for example `role:ckpt` and `role:checkpoint`.

`shard_count` is how many files the model spans: a checkpoint split into
`model-00001-of-00040.safetensors` and its siblings is one row whose `size` is the whole
model, not forty unclassifiable rows. `path_count` stays per file, because deduplication
and serving are per file. In a facet bucket, `token` is what a click toggles in the query
and `query` is the whole query that selects exactly that bucket.

A row also carries `incomplete`, true when the set found fewer files than its own names or
its index document declare. `shard_count` is then the number of files present, which is one
for a lone shard, and `size` is what is on disk rather than what the whole set would weigh.

A page can hold fewer rows than `limit`, and the rows of every page together fewer than
`total`. The index is answered from even while a rescan has logged changes no refresh has
applied, so a row whose entry that rescan replaced is skipped rather than the whole request
being refused; `total` stays what the index counted, and the refresh that follows the rescan
brings the row back. A skipped row still counts as a position walked, so `next_cursor` is
offered exactly as it would have been had the row been assembled and paging never stops
short. The server logs the window at `debug`; nothing in the response names it.

A token carrying no `key:` is free text, and it is matched against the words the index holds:
the value is cut where the index cuts a name, and those pieces have to appear consecutively
and in that order with the last one matched as a prefix, so `gpt-oss`, `Q4_K_M` and
`Qwen3-VL-8B` each find the file whose name they were pasted from, and `pony`, `souls` and
`8B` each find the file they sit inside. A name is cut at every character that is neither a
letter nor a digit; before an uppercase letter that follows a lowercase one, unless another
uppercase letter follows it, so `cyberrealisticPony` is `cyberrealistic` and `Pony` while
`LoRA` and `PONYTAIL` stay one word each; and at every step between a letter and a digit, so
`v160` is `v` and `160`. Every word is lowercased, and a word of forty bytes or more is
dropped and leaves its position behind for the phrase to step over.
Words separated by a space are each required, in any order, and a `~` in front of a token
relaxes it to every word matched as a prefix in any order. A value of one word is matched
without any cap, while in a value of several words the last word expands to at most 1024 index
terms in dictionary order, so a very short last word on a very large catalog can miss a row it
names; typing one more letter narrows the expansion back under the cap. The limit this leaves
is that a word is matched from its start: a run from the middle of one unbroken word does not
match, so `souls` finds `loxsSoulsMixPerfectDoll_v30` and `oulsmix` and `ulsMixPer` do not.

A name, an org, a base, the trigger words and the tensor keys are matched from the start of a
word this way. A revision's card is not: it is a field of its own, cut by the same tokenizer,
and a free-text word is matched against it whole, so `wan 2.2` finds the Wan files and no
card that says `want`, and a phrase is matched whole word for whole word in the card and as a
phrase prefix in the name. A word matching either side answers the row.

A row carries `shard_total`, how many files the set declared at scan time, `null` for a
single file and for a set nothing declared a total for. `shard_count` counts the files
present, so a set that declares more than it holds reads `incomplete` with the two numbers
apart.

`GET /api/v1/search/parse?q=` returns only `query`, for the token strip while typing.

## Artifacts and blobs

- `GET /api/v1/artifacts/{id}`: row fields plus `facts` (ordered key/value pairs exactly
  as the inspector shows them, `role base format size node layout blob` and then, when there
  is one, `files` - `4 shards` for a whole set, `1 of 15 shards, incomplete` for one short of
  its declared total, `1 shards, incomplete` where nothing declared a total, and nothing at
  all for a single whole file - then `civitai`, the model and version a listing named these
  bytes as, and then the role-specific rows; the confirm and triage routes answer the same
  order with no `civitai` row), `evidence` (`[{ "field", "value", "source",
  "detail", "confidence" }]`), `entries` (`repo`, `revision`, `path`, `blob_path`, `node`, `state`,
  `symlink`; `path` is the path a person walked, and `blob_path` names the shortened cache
  blob it links to when that is somewhere else, otherwise `null`),
  `duplicates`, `plugs_into` (first four compatible artifacts), `usage` summary,
  `override_applied`.
- `GET /api/v1/artifacts/{id}/compat?limit=`: `{ "rows", "total", "rule", "heading",
  "banner" }`. A row carries `artifact_id`, `role`, `role_code`, `org`, `prefix`, `name`,
  `repo`, `detail`, `size`, `size_human`, `state`, `state_label`, `pull_progress`; `total` is how
  many candidates the rule accepted, which can exceed `rows.len()` when `limit` truncates.
- `GET /api/v1/artifacts/{id}/pull-command`: `{ "git": null,
  "hf": "HF_ENDPOINT=http://hoard.lan:7099 hf download org/name 'file.safetensors'",
  "repository": "HF_ENDPOINT=http://hoard.lan:7099 hf download org/name",
  "path": "/srv/..." }`. The `hf` command appends ` --repo-type dataset|space` for a repo of
  that type; models carry no flag. `git` is a `git clone` line only when the repository's
  source is a git clone this node can serve over the git face, and `null` otherwise; `hf`
  appends the artifact's own path, or the `--include` globs that name a shard set
  (`"<stem>-*-of-<total>.<ext>"`, and `"<stem>.<ext>.index.json"` only for a `safetensors` or
  `bin` set, because no other container ships an index document), or nothing at all for the
  repository-level command the generated card prints.
  `hf` carries ` --revision <sha>`, or the revision's name when the index holds no sha for it,
  after the repo type and before the file scope, when the artifact is held in a revision other
  than `main`, because a detached snapshot's files are not in the revision the CLI asks for by
  default and the command would download nothing.
  An incomplete set is scoped by the set's two `--include` globs and not by the one shard the
  disk already holds, because that line is the one that completes the download through this
  node; a lone shard of a set is therefore nobody's companion.
  An LLM held beside exactly one projector in the same repository and revision, and that
  projector, name both files in the one line, because neither is usable without the other.
  `repository` is the same line with no file scope, for pulling everything the repository
  holds; it is `null` when the command names no artifact, `null` for a category folder of
  a tool layout, which is nobody's repository and whose whole-folder pull would be every LoRA
  on the disk, and `null` when it would repeat the `hf` line word for word. A repository is a
  category folder when loose discovery made it from a category folder of a layout it
  recognized and stored that layout on the record, `comfy/loras` under a ComfyUI root. A
  per-model folder of a plain loose source, `bulk/gpt-oss-120b` under a source named `bulk`,
  is a repository a person can pull whole even though its org is that source's own name, and
  so is a model folder under a Kohya training root or a root holding a `.gguf`, whose top
  level folders are models rather than categories: there, only the files dropped directly at
  the root are a category folder.
  `path` is the path a person walked to the file, which for an hf-cache artifact is the
  snapshot link rather than the blob under `blobs/`.
  A file name is always quoted for the shell. The repository reference and the
  `HF_ENDPOINT=` value are quoted only when they hold a character outside
  `[A-Za-z0-9._/:-]`, which a repository named after a directory on disk can, so a line a
  script carries stays one a person can read.
- `PUT /api/v1/artifacts/{id}/override { "role", "base", "sub_role" }`, at least one of the
  three set or 400. `DELETE` clears.
- `GET /api/v1/blobs/{sha256}`: `{ "sha256", "size", "size_human", "path_count",
  "first_seen", "hashed_at", "entries" }`, `entries` in the same shape as an artifact's.

## Triage

- `GET /api/v1/triage`: artifacts still waiting for a verdict, with `guess` (`role`, `base`,
  `confidence`), `header` (tensor count, key prefixes, hidden dim, dtype) and `reason`, the
  sentence naming why the artifact is queued: `role unclassified`, `role vae at 0.51, below
  0.85`, or `1 of 15 shards present, incomplete` for one shard of a set whose index document
  names fifteen; a set that declared no total reads `3 shards present, incomplete`. A verdict
  answers the role questions and cannot make a set complete, so an incomplete artifact stays
  in this queue after one, with the incomplete sentence as its reason. The count, the prefixes and
  the dtype are what the sniff measured over the whole file; the hidden dimension is read
  from the at most sixteen tensor samples the stored header keeps, thinned by a stride from
  the sniff's draw rather than taken off its front, not from every tensor the file holds. Inference itself ran over all of them before the row was written.
- `POST /api/v1/triage/{id}/confirm { "role", "base", "sub_role" }`: assigns the role, the
  sub-role and the base and, when the role it assigns settles the record, settles the
  artifact's paths from `triage` to `local` in the same transaction a rejection uses, so the
  queue, the `state:triage` shelf and the stats triage total drop together. A record that
  still needs triage after the verdict, an incomplete set or one whose role is left `unk`,
  stays with its paths in `triage` and stays in all three: a verdict naming only a base leaves
  the artifact queued, counted and shelved alike, because the role question is still open.
  `sub_role` is optional.
- `POST /api/v1/triage/{id}/reject`: records the path a scan walks as noise, which for an
  hf-cache file is the snapshot link rather than the blob, so later scans skip it. The
  rejection is stored on the artifact and settles its paths in the same transaction, so the
  artifact leaves this queue, the `state:triage` shelf and the stats triage total at once. An
  incomplete set leaves the queue this way or by its missing files landing, and no other.
- `GET /api/v1/triage/noise` and `DELETE /api/v1/triage/noise/{id}`. A noise row names the
  artifact it was rejected from in `artifact_id`, `null` for a path recorded any other way.
  The delete removes the row so the next scan walks the path again and, when the row named an
  artifact no other noise row still names, clears that artifact's rejection at once: when the
  artifact still needs a person, its `local` paths go back to `triage` (a `served` path keeps
  its mark and takes the flag) and the queue, the shelf and the stats total rise together;
  when a confirm had already settled its role, only the flag clears and nothing moves. It
  answers `{ "id", "removed", "artifact_id" }`, `artifact_id` being the artifact it restored
  or `null`, and 404s for an id that was no row.

## Repos and README

- `GET /api/v1/repos?q=&type=&cursor=`: `{ rows, next_cursor, total }`, paged by an
  opaque row cursor.
- `GET /api/v1/repos/{type}/{org}/{name}`: header fields (`artifact_id`, `role`,
  `role_code`, `role_full`, `state`, `state_full`, `pull_progress`, `meta`, `revision`,
  `revision_label`, `revision_sha`), `refs` (`name`, `sha`, `when`, `kind`, `current`),
  `revision_summary`
  (`14 files · 3 branches · 2 tags`), `facts`, `paths`, `pull` commands, and `rows` (every
  artifact of the repo, in the search row shape). `remote` is the URL a clone's `origin`
  names, or `null` for a repository the mirror, a peer or a bare folder filed.
  `revision` is the shown revision's name and `revision_label` is what the REVISION chip
  prints: `refs/heads/main` or `refs/tags/v1.0` for a revision a ref points at, and
  `613bd8d (detached)` for one no ref names. `meta` measures what the header describes: the
  opened artifact's own bytes, or the shown revision's when no artifact was named. With no
  `artifact`, the header describes the largest weights file **of the shown revision**.
- `artifact=<id>` on this route and the three below names the artifact the page was opened
  on. It
  replaces the largest weights file as what the header, `facts`, `meta`, `paths`, `pull` and
  `compat` describe, and when no `revision` is asked for the page opens on the revision that
  holds it rather than on `main`. An explicit `revision` still wins. An id the repository does
  not hold, one whose revision this record does not hold (a peer's copy of the same
  repository is filed under the peer's own source and can carry a commit only that peer has),
  an unknown one and an unparsable one are ignored rather than refused, so the page
  opens either way and behaves as it does with no `artifact` at all.
- `GET /api/v1/repos/{type}/{org}/{name}/files?revision=&artifact=`: rows `path`, `kind`
  (config, weights, single-file, card, git, group), `size`, `state` (present, deduped,
  lfs_ptr, absent), `artifact_id`, and `opened`, true on every file the named artifact is
  made of, which for a shard set is every member. `revision` is the revision the rows come
  from, which is the opened artifact's own when the request named no revision.
- `GET /api/v1/repos/{type}/{org}/{name}/readme?revision=&artifact=`: `{ "frontmatter": [{
  "key", "value" }], "html": "<rendered>", "generated": false, "meta": "4.1 KB · 118 lines",
  "revision" }`. A generated card describes the opened artifact, and prints its "Pull it"
  block for every repository but a category folder of a tool layout.
- `GET /api/v1/repos/{type}/{org}/{name}/compat?revision=&artifact=`: as artifacts compat.
- `GET /api/v1/repos/{type}/{org}/{name}/usage`: `{ "days": [ { "date", "pulls" } ],
  "pulls": [ { "when", "at", "who", "what", "bytes", "rate_bps" } ], "clients": 3,
  "total": 41 }`. `when` is the relative spelling the design prints, `at` the instant
  behind it. `days` is the seven day window, read off the repository's own record,
  which every usage write moves, rather than from its log; `clients` is the distinct clients over the whole history, as
  the repository's own record has counted them, exact up to a cap of 4,096 addresses and
  reported as that cap beyond it; `pulls` is the newest twenty rows, newest first. A client
  is the first `x-forwarded-for` or `x-real-ip` value that parses as an IP address,
  otherwise the socket the request arrived on.

## Shelves

`GET /api/v1/shelves`, `POST { "name", "query" }`, `PUT /{id}`, `DELETE /{id}`. Seeded
shelves carry `"builtin": true`; `name` may not be empty (400), and deleting a builtin
shelf is 409, not 404 or 400.

A listed shelf carries `count`, how many artifacts its query answers: the same number
`GET /api/v1/search` reports as `total` for that query, counted without the facet rail and
without a page. `0` means the query answers nothing. `null` means it was not counted, for
any reason - the query does not compile, the search index is not built yet, the catalog
refused it - or because the shelf came back from a write, which answers with the row it
wrote rather than re-running its query. One shelf that cannot be counted never stops the
rest being counted.

## Loadouts

- `GET /api/v1/loadouts`, `GET /api/v1/loadouts/{id}`: document plus resolution per line
  (`present`, `elsewhere` with the node, `missing`) and `apply_preview`
  (`{ "present": 3, "pulling": 2, "bytes": 1200000000 }`). An item carries `prefix`, what the
  drawer prints dimmed before `name`, in the same spelling a results row uses; it is empty
  when the pin resolved to nothing. A pin whose only local row is an incomplete set resolves
  `missing`, or `elsewhere` when a peer holds a hashed copy, and never `present`: one shard of
  fifteen cannot satisfy the line, so apply pulls it rather than linking it.
- `POST /api/v1/loadouts { "text": "<toml>" }`, the structured document, or `{}` or a blank
  `name`, which takes the next free `loadout-N`. A name another loadout already holds is 409
  `Conflict` on every route, never merged, with the message
  `invalid loadout: a loadout named "night" already exists`.
- `PUT /api/v1/loadouts/{id} { "name" }` renames one loadout, inside its document as well as
  on its row, answering the loadout view. A blank name is 400, a taken name 409, an unknown
  id 404; the loadout's own current name is accepted and changes nothing.
- `DELETE /api/v1/loadouts/{id}` answers `{ "id", "deleted" }`; an unknown id is 404. An
  apply already running against that loadout fails at its next read of it, because a job row
  names the loadout only in its subject.
- `POST /api/v1/loadouts/{id}/items { "artifact_id", "weight", "on_second_host" }`,
  `DELETE .../items/{artifact_id}`. `on_second_host` is `replace` (the default) or `branch`:
  `branch` on a line that would displace a host starts a new loadout named after the file,
  by its name without a letter extension, numbered `-2`, `-3` when that name is taken, and
  pins the line there instead. The answer is the loadout view of whichever loadout now holds
  the line, plus `replaced` - `{ "slot", "name" }` for a host, VAE or upscaler line that was
  occupied, otherwise `null` - and `notice`, the one line the drawer prints:
  `+ sdxl-base-1.0 pinned at v3`, `replaced vae old-vae with sdxl-vae`,
  `new loadout "cyberrealisticPony_v160": checkpoint pinned`, or
  `sdxl-vae is already in night` for a line the loadout already holds, which writes nothing.
  A weight change is `DELETE` then `POST`, because a re-pinned line is answered rather than
  rewritten. A line is identified by its repository and path together, so the same file name
  under two repositories is two lines: both pin, and `DELETE .../items/{artifact_id}` removes
  only the line whose repository the named artifact sits in.
- `hoard loadout import` is the one path that replaces by name: reading a document whose name
  a loadout already holds overwrites that loadout, where every route above refuses with 409.
- `POST /api/v1/loadouts/{id}/validate`: compat violations.
- `POST /api/v1/loadouts/{id}/apply { "node", "profile" }`: starts a job, answering
  `{ "job_id", "node", "apply_preview" }` with the same `apply_preview` shape as loadout
  detail, resolved before the pull runs.
- `GET /api/v1/loadouts/{id}/export?format=toml|hf-script|comfy`. An item carries
  `abs_path`, the path on this node holding the whole artifact, `null` when none does; the
  `comfy` script links that path and writes `# <name>: not on this node` for a line nothing
  here holds, so it never links a path that resolves to nothing. The `hf-script` writes one
  `hf download` line per item in the spelling `GET /api/v1/artifacts/{id}/pull-command` gives,
  without the companion file, so a pinned shard set exports its `--include` globs, a pinned
  dataset its `--repo-type` and a line pinned outside `main` its `--revision`. A pin whose
  content resolves in another repository - the same bytes filed twice - exports the pin's own
  repository and path, never one repository's reference beside another's file.

## Projection

`POST /api/v1/link { "query" | "shelf_id", "profile": "comfyui|a1111|llama", "dir" }`
writes symlinks and returns the list written; the CLI `hoard link` calls this.

`dir` is optional and, when relative, resolves under `<data-dir>/profiles/<profile>`. An
absolute path is accepted only when it lies under that root or under a directory named in
`[server] projection_roots`, so a caller cannot choose where a node writes. `apply` takes
no `dir` at all and always uses the configured root.

## Sources, scans, jobs

- `POST /api/v1/sources { "path": "/absolute/server/path", "name": "optional-name",
  "kind": "auto" }`: validates a directory on the server, persists it in
  `<data_dir>/sources.json`, admits it to the scan lane and watcher, and queues its first
  scan. Returns a source view. `kind` defaults to `auto` and also accepts `git`, `loose`
  and `hf-cache`; a missing or blank name uses the directory's name. Relative paths, reserved
  names and overlap with managed storage are rejected; duplicate names and overlap with
  existing sources return 409. Configured folders are listed before their first scan.
  There is currently no source-removal route. Source IDs are positional listing IDs;
  use the source's name when requesting a scan.
- `GET /api/v1/sources`: kind, files, bytes, last scan, `pending_hashes` and the
  `pending_bytes` those files hold. There is no blob count: the catalog counts content by
  artifact rather than by blob, and a column that always read zero is not one.
  The list holds every source the catalog knows, including the ones this node does not
  scan: the mirror's downloads, the puller's landings and a peer's namespace. `can_scan` says
  whether `POST /api/v1/scan` will accept the row, which is true exactly when the
  configuration names it; `managed_by` says who writes it instead, one of `"downloads"`,
  `"pulls"`, `"peer"` or `"nobody"`, and is absent when `can_scan` is true. `"nobody"` is a
  source the catalog still holds that the configuration no longer names, which is what a
  dropped or renamed `[[index.sources]]` leaves behind. It lasts until the next
  whole-configuration rescan, which deletes the source, its repositories, its entries and
  the history it served. A
  client offers the scan action on `can_scan` alone and never on a name it recognises.
- `POST /api/v1/scan { "source" | null, "hash": "now" | "later" | "never" }` returns
  `{ "job_id" }`. `"background"` is accepted as an alias for `"later"`; omitting `hash`
  also means `"later"`. Every scan under `serve` runs on one lane, one at a time, in the
  order asked, so a request answers its job id at once but may wait behind another. A
  request naming a source that is already queued and has not started answers the job id it
  merged into, and that queued scan takes the stronger of the two hash policies, `"now"`
  over `"later"` over `"never"`. A node shutting down runs what the lane still holds before
  it exits.
- `GET /api/v1/jobs?state=`: scans, hash runs, pulls, applies and lookups with `phase`,
  `progress` (0..1), `bytes`, `total`, `rate_bps`, `started`, `finished`, `error`. A scan's
  hashing phase reports `total` as the bytes it was owed when the pass opened. A job of kind
  `lookup` has subject `civitai` and reports how many hashes of its window it has asked about
  out of how many that window holds; it is failed with the reason when the service rate limits
  it and cancelled when the node stops mid-pass. `GET /api/v1/jobs/{id}`,
  `POST /api/v1/jobs/{id}/cancel`. Cancellation refuses already-finished jobs with 409;
  there is currently no deletion route for completed scan history.

## The lookup

`[lookup] civitai = true` in `hoard.toml` lets a background worker ask civitai.com by sha256
what each hashed checkpoint, LoRA, ControlNet, VAE, embedding and upscaler on this node is,
`GET https://civitai.com/api/v1/model-versions/by-hash/<sha256>`: one request per hash, one
more every thirty days while the service does not know that hash, and at most one request a
second. The sha256 is the whole request: no path, no file name, no `Authorization` header, no
query string, one attempt per hash per pass, and no redirect followed, so a 3xx answers
nothing rather than carrying the hash on to another host. The switch is off by default and is
ignored while `[mirror] offline = true`, so an offline node opens no socket whatever the
switch says. A pass runs when the node starts and no scan is running or queued, again as soon
as a scan has finished - a scan is what restores the hashes a rebuilt catalog is asked about -
and otherwise every fifteen minutes; while a scan runs the worker looks again every ten
seconds and asks nothing.

An answer naming a base hoard knows refines the stored one, `sdxl` becoming `pony` or
`illustrious`, and names one when hoard proved nothing. A label is the spelling the service
writes, `Illustrious` or `Flux.2 Klein 4B`, except on a key hoard's own rules produce, which
keeps the spelling those rules give it and is stored with no label at all, so one key is
never printed two ways on the rail or in the BASE column. An answer naming another
architecture loses to what hoard already has, and a user's verdict is never overwritten - an
artifact a verdict has settled the base of is never asked about at all. Every answer leaves
one evidence row on the artifact with source `lookup`, field `base` and a detail sentence
saying what was listed and what it did, which `GET /api/v1/artifacts/{id}` returns beside the
tensor and header evidence, and the listing itself is kept on the artifact, which is what the
`civitai` fact prints. An answer listing `trainedWords` writes them as the artifact's trigger
words when nothing stronger than a file-name or front-matter guess is stored, leaves a second
lookup evidence row on the field `trigger`, and never displaces a trigger read from
`ss_tag_frequency`. Answers are remembered in `<data_dir>/civitai.redb`, at memo version 2, so
a catalog rebuilt after a format change restores them without asking again; a memo of the
previous version is deleted and asked again once.

## Nodes and peers

- `GET /api/v1/nodes`: `[{ "id", "name", "url", "online", "indexed_bytes", "last_seen",
  "self": true }]`.
- `GET /api/v1/peers/digest?since=<cursor>&limit=`: compact rows `{ "sha256", "size",
  "role", "base", "repo", "revision_sha", "path", "state" }` plus `next_cursor`, consumed by
  other nodes. A digest says what a node holds and never what it dropped, so a reader also
  re-reads the whole digest hourly and retires the rows that pass did not name; without that
  a REMOTE row would outlive the file it points at and keep offering a pull that fails.
- `POST /api/v1/peers { "url", "name" }` adds a static peer; `name` is optional and defaults
  to the name the peer reports about itself. Answers the created node in the `/api/v1/nodes`
  row shape.

## System

- `GET /healthz`: `{ "status": "ok" | "degraded", "index": "ok" | "error",
  "upstream": "ok" | "offline" | "unreachable", "watch": "off" | "ok" | "degraded",
  "peers": 3 }`. It answers 200 even when degraded: a health route that fails to respond
  tells a watchdog nothing. `watch` is `off` when this node is configured not to watch its
  sources or has none to watch, and `degraded` when the platform refused a watch on a
  configured root, which degrades `status` with it while manual scans keep working. A
  configured root that is not a directory is not degraded: a scan skips it with a warning
  too.
- `GET /api/v1/stats`: `{ "artifacts", "blobs", "repos", "triage" }` totals, `indexed_bytes`
  (and `indexed_bytes_human`), `roles`/`formats`/`nodes` facet breakdowns, and `cache`
  (`bytes`, `bytes_human`, `limit`, `limit_human`).
- `GET /api/v1/settings` and `PUT`: density (`Condensed|Roomy`), role color mode
  (`Color-coded|Monochrome`), inspector visibility and theme (`system|dark|light`), also
  mirrored in `localStorage` by the UI. A value outside its vocabulary is a 400.
