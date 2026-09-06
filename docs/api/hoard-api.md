# Hoard JSON API (`/api/v1`)

Consumed by the web UI, the CLI and peer nodes. Namespaced under `/api/v1` so it never
collides with the Hub-compatible `/api/{models,datasets,spaces}` routes. Responses are
JSON; errors are `{ "error": { "code": "<PascalCase>", "message": "<text>" } }` with a
matching status. Mutating routes require `Authorization: Bearer <HOARD_NODE_TOKEN>` when
that variable is set; read routes are open on the LAN.

Field vocabularies are the grammar spellings: `role` in `ckpt lora ctrl upsc vae emb llm
ds unk`, `sub_role` in `ipa tok mot` or null, `state` in `served local pulling stale
remote triage`, `format` in `safetensors gguf diffusers transformers pth pt bin onnx
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
                "token": "role:lora", "query": "role:lora" } ],
    "base": [], "format": [], "precision": [], "node": [],
    "disk_pressure": { "reclaimable_bytes": 333000000000, "duplicate_blobs": 47,
                        "label": "310.0 GB reclaimable — 47 duplicate blobs" } },
  "rows": [ {
    "artifact_id": "a_01H...", "blob_sha256": "9f2c...41ab", "role": "lora", "sub_role": null,
    "role_confidence": 0.98, "org": "kohya", "name": "film-grain-35mm", "base": "sdxl 1.0",
    "format": "safetensors", "precision": "fp16", "size": 239075328, "rank": 32, "alpha": 16,
    "params": null, "context": null, "quant": null, "revision": "main", "revision_sha": "462c3cb...",
    "node": "workstation", "state": "served", "pull_progress": null,
    "trigger": "film grain, 35mm, halation", "path": "/mnt/vault/loras/film-grain-35mm.safetensors",
    "path_count": 2, "shard_count": 1, "last_used": "2026-09-04T09:10:00Z" } ],
  "next_cursor": "...", "total": 212, "nodes": 3, "summary": "212 ARTIFACTS · 3 NODES"
}
```

`shard_count` is how many files the model spans: a checkpoint split into
`model-00001-of-00040.safetensors` and its siblings is one row whose `size` is the whole
model, not forty unclassifiable rows. `path_count` stays per file, because deduplication
and serving are per file. In a facet bucket, `token` is what a click toggles in the query
and `query` is the whole query that selects exactly that bucket.

`GET /api/v1/search/parse?q=` returns only `query`, for the token strip while typing.

## Artifacts and blobs

- `GET /api/v1/artifacts/{id}`: row fields plus `facts` (ordered key/value pairs exactly
  as the inspector shows them), `evidence` (`[{ "field", "value", "source", "detail",
  "confidence" }]`), `entries` (`repo`, `revision`, `path`, `node`, `state`, `symlink`),
  `duplicates`, `plugs_into` (first four compatible artifacts), `usage` summary,
  `override_applied`.
- `GET /api/v1/artifacts/{id}/compat?limit=`: `{ "rows", "total", "rule", "heading",
  "banner" }`. A row carries `artifact_id`, `role`, `role_code`, `org`, `name`, `repo`,
  `detail`, `size`, `size_human`, `state`, `state_label`, `pull_progress`; `total` is how
  many candidates the rule accepted, which can exceed `rows.len()` when `limit` truncates.
- `GET /api/v1/artifacts/{id}/pull-command`: `{ "git": "git clone http://hoard.lan:7099/org/name",
  "hf": "HF_ENDPOINT=http://hoard.lan:7099 hf download org/name", "path": "/srv/..." }`. The
  `hf` command appends ` --repo-type dataset|space` for a repo of that type; models carry no
  flag.
- `PUT /api/v1/artifacts/{id}/override { "role", "base", "sub_role" }`, at least one of the
  three set or 400. `DELETE` clears.
- `GET /api/v1/blobs/{sha256}`: `{ "sha256", "size", "size_human", "path_count",
  "first_seen", "hashed_at", "entries" }`, `entries` in the same shape as an artifact's.

## Triage

- `GET /api/v1/triage`: unclassified artifacts with `guess` (`role`, `base`, `confidence`)
  and `header` (tensor count, key prefixes, hidden dim, dtype).
- `POST /api/v1/triage/{id}/confirm { "role", "base", "sub_role" }`: assigns and leaves
  triage; `sub_role` is optional.
- `POST /api/v1/triage/{id}/reject`: records the path as noise; later scans skip it.
- `GET /api/v1/triage/noise` and `DELETE /api/v1/triage/noise/{id}`.

## Repos and README

- `GET /api/v1/repos?q=&type=&cursor=`: `{ rows, next_cursor, total }`, paged by an
  opaque row cursor.
- `GET /api/v1/repos/{type}/{org}/{name}`: header fields (`artifact_id`, `role`,
  `role_code`, `role_full`, `state`, `state_full`, `pull_progress`, `meta`, `revision`,
  `revision_sha`), `refs` (`name`, `sha`, `when`, `kind`, `current`), `revision_summary`
  (`14 files · 3 branches · 2 tags`), `facts`, `paths`, `pull` commands, and `rows` (every
  artifact of the repo, in the search row shape).
- `GET /api/v1/repos/{type}/{org}/{name}/files?revision=`: rows `path`, `kind` (config,
  weights, single-file, card, git, group), `size`, `state` (present, deduped, lfs_ptr, absent),
  `artifact_id`.
- `GET /api/v1/repos/{type}/{org}/{name}/readme?revision=`: `{ "frontmatter": [{ "key",
  "value" }], "html": "<rendered>", "generated": false, "meta": "4.1 KB · 118 lines",
  "revision" }`.
- `GET /api/v1/repos/{type}/{org}/{name}/compat?revision=`: as artifacts compat.
- `GET /api/v1/repos/{type}/{org}/{name}/usage`: `{ "days": [ { "date", "pulls" } ],
  "pulls": [ { "when", "at", "who", "what", "bytes", "rate_bps" } ], "clients": 3,
  "total": 41 }`. `when` is the relative spelling the design prints, `at` the instant
  behind it.

## Shelves

`GET /api/v1/shelves`, `POST { "name", "query" }`, `PUT /{id}`, `DELETE /{id}`. Seeded
shelves carry `"builtin": true`; `name` may not be empty (400), and deleting a builtin
shelf is 409, not 404 or 400.

## Loadouts

- `GET /api/v1/loadouts`, `GET /api/v1/loadouts/{id}`: document plus resolution per line
  (`present`, `elsewhere` with the node, `missing`) and `apply_preview`
  (`{ "present": 3, "pulling": 2, "bytes": 1200000000 }`).
- `POST /api/v1/loadouts { "text": "<toml>" }` or the structured form.
- `POST /api/v1/loadouts/{id}/items { "artifact_id", "weight" }`, `DELETE .../items/{artifact_id}`.
- `POST /api/v1/loadouts/{id}/validate`: compat violations.
- `POST /api/v1/loadouts/{id}/apply { "node", "profile" }`: starts a job, answering
  `{ "job_id", "node", "apply_preview" }` with the same `apply_preview` shape as loadout
  detail, resolved before the pull runs.
- `GET /api/v1/loadouts/{id}/export?format=toml|hf-script|comfy`.

## Projection

`POST /api/v1/link { "query" | "shelf_id", "profile": "comfyui|a1111|llama", "dir" }`
writes symlinks and returns the list written; the CLI `hoard link` calls this.

`dir` is optional and, when relative, resolves under `<data-dir>/profiles/<profile>`. An
absolute path is accepted only when it lies under that root or under a directory named in
`[server] projection_roots`, so a caller cannot choose where a node writes. `apply` takes
no `dir` at all and always uses the configured root.

## Sources, scans, jobs

- `GET /api/v1/sources`: kind, files, bytes, last scan, pending hashes; `blobs` is kept for
  the shape and always reads zero, the catalog counts content by artifact rather than by
  blob. The list holds every source the catalog knows, including the ones this node does not
  scan: the mirror's downloads, the puller's landings and a peer's namespace. `can_scan` says
  whether `POST /api/v1/scan` will accept the row, which is true exactly when the
  configuration names it; `managed_by` says who writes it instead, one of `"downloads"`,
  `"pulls"`, `"peer"` or `"nobody"`, and is absent when `can_scan` is true. `"nobody"` is a
  source the catalog still holds that the configuration no longer names, which is what a
  dropped or renamed `[[index.sources]]` leaves behind: nothing removes a `Source` row. A
  client offers the scan action on `can_scan` alone and never on a name it recognises.
- `POST /api/v1/scan { "source" | null, "hash": "now" | "later" | "never" }` returns
  `{ "job_id" }`. `"background"` is accepted as an alias for `"later"`; omitting `hash`
  also means `"later"`.
- `GET /api/v1/jobs?state=`: scans, hash runs, pulls, applies with `phase`, `progress`
  (0..1), `bytes`, `rate_bps`, `started`, `finished`, `error`. `GET /api/v1/jobs/{id}`,
  `POST /api/v1/jobs/{id}/cancel`.

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
  "upstream": "ok" | "offline" | "unreachable", "peers": 3 }`. It answers 200 even when
  degraded: a health route that fails to respond tells a watchdog nothing.
- `GET /api/v1/stats`: `{ "artifacts", "blobs", "repos", "triage" }` totals, `indexed_bytes`
  (and `indexed_bytes_human`), `roles`/`formats`/`nodes` facet breakdowns, and `cache`
  (`bytes`, `bytes_human`, `limit`, `limit_human`).
- `GET /api/v1/settings` and `PUT`: density (`Condensed|Roomy`), role color mode
  (`Color-coded|Monochrome`), inspector visibility and theme (`system|dark|light`), also
  mirrored in `localStorage` by the UI. A value outside its vocabulary is a 400.
