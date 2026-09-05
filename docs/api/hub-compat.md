# HuggingFace Hub compatibility contract

Hoard implements the subset of the Hub HTTP API that `huggingface_hub`, the `hf` CLI,
`snapshot_download`, diffusers, transformers and llama.cpp loaders rely on. The contract is
derived from the Python reference `olah` (`/home/johnz86/GIT/olah`) and from observed
`huggingface.co` behaviour. Where olah has quirks that are accidents rather than protocol,
the "Deviations" section says what hoard does instead.

Clients point at hoard with `HF_ENDPOINT=http://<host>:7099` (default port from the design;
olah used 8090). Peer nodes use the same face, so a peer pull is an ordinary resolve.

## Vocabulary

- `repo_type`: `models`, `datasets`, `spaces`. Anything else is 404.
- Repo reference forms accepted on every route family:
  - `/{repo_type}/{org}/{repo}` and `/{repo_type}/{org_repo}` (canonical)
  - `/{org}/{repo}` (implies `models`)
  - `/{repo}` single segment (implies `models`, no org)
- `{revision}`: branch, tag or 40-hex commit sha. Every response that touched a revision
  carries `x-repo-commit: <resolved sha>`.
- Path segments are cleaned: `\` becomes `/`, repeated `/` collapse, any `..` component is
  rejected with 404 (olah stripped it as a substring; hoard rejects instead).

## Routes

### Repo metadata

| Route | Methods |
| --- | --- |
| `/api/{repo_type}/{ref}` | HEAD, GET |
| `/api/{repo_type}/{ref}/revision/{revision}` | HEAD, GET |

Without `/revision`, the tip of the default branch is resolved first. Body:

```json
{
  "_id": "<sha256 of org/repo/sha>", "id": "org/repo", "author": "org",
  "sha": "<commit>", "lastModified": "2026-01-17T10:22:31.000000Z",
  "private": false, "gated": false, "disabled": false,
  "tags": [], "description": "<README body without frontmatter>",
  "paperswithcode_id": null, "downloads": 0, "likes": 0,
  "cardData": { "...": "README YAML frontmatter or null" },
  "siblings": [{ "rfilename": "model.safetensors" }],
  "createdAt": "<earliest commit>"
}
```

`siblings` lists every file recursively, no directories. Upstream responses are passed
through verbatim (status, headers, still-compressed body) when proxied.

### Tree

`/api/{repo_type}/{ref}/tree/{revision}/{path}` HEAD, GET. Query `recursive` and `expand`
(`true`/`false`). Body is a JSON array:

```json
[
  { "type": "directory", "oid": "<git tree sha1>", "size": 0, "path": "unet" },
  { "type": "file", "oid": "<git blob sha1>", "size": 4265146304,
    "path": "unet/diffusion_pytorch_model.safetensors",
    "lfs": { "oid": "<sha256>", "size": 4265146304, "pointerSize": 135 } }
]
```

`oid` is always the git blob sha1 (hoard computes `sha1("blob <size>\0" + bytes)` for
non-LFS files and stores it in the index). LFS files carry the `lfs` object with the sha256
`oid`. `expand=true` adds `lastCommit {id,title,date}` and a static `security` object.

### paths-info

`/api/{repo_type}/{ref}/paths-info/{revision}` HEAD, POST. Form field `paths` repeated.
Returns a flat JSON array of the same entry shape as tree, plus optional `xetHash`. Missing
`paths` field is 422. Hoard answers from the index in one query rather than one upstream
call per path.

### Commits

`/api/{repo_type}/{ref}/commits/{revision}` HEAD, GET. Newest first, at most 50:

```json
[{ "id": "<sha>", "title": "<message>", "message": "",
   "authors": [{ "name": "<author>", "avatar": null }],
   "date": "2026-01-17T10:22:31.000000Z" }]
```

Index-synthesized repos (loose files, HF cache snapshots) expose exactly one commit per
revision.

### Resolve (file bytes)

`/{ref}/resolve/{revision}/{path}` HEAD and GET, all three ref forms. Also the
`/{repo_type}/{ref}/resolve/...` form for datasets and spaces.

Request headers honoured: `Authorization` (forwarded upstream only), `Range` (`bytes=a-b`,
`a-`, `-n`, comma lists; unsatisfiable ranges dropped; zero satisfiable ranges gives 416 with
`content-range: bytes */<size>`).

Response headers:

| Header | Value |
| --- | --- |
| `accept-ranges` | `bytes` |
| `x-repo-commit` | resolved sha |
| `etag` | `"<sha256>"` quoted for LFS-sized files, `"<git sha1>"` for small files |
| `x-linked-etag` | `"<sha256>"` when the file is LFS |
| `x-linked-size` | byte size when the file is LFS |
| `content-length` | full size, or the range length |
| `content-range` | `bytes s-e/size` on 206 |
| `content-type` | `multipart/byteranges; boundary=...` for multi-range, else guessed |

HEAD returns the identical headers and status as GET with an empty body, and never writes
to the cache. Status 200 without Range, 206 with, 404 `EntryNotFound` when the path is not in
the revision.

### LFS and CDN pass-through

`/repos/{d1}/{d2}/{hash_repo}/{hash_file}` HEAD, GET and `/{ref}/{hash_file}` HEAD, GET
exist so clients that were redirected to a CDN URL rewritten to the mirror still work.
Hoard serves these from the block cache keyed by content hash. No `x-repo-commit`.

### Auth pass-through

`GET /api/whoami-v2` forwards headers to upstream and returns its answer. Offline mode
returns a synthetic anonymous identity so `hf auth whoami` does not hang.

### Xet

`/api/{repo_type}/{ref}/xet-read-token/{revision}` and `/xet-bridge-us/{repo_hash}/{hash}`
are proxied as in olah. Hoard strips `x-xet-hash` from resolve responses it serves itself so
`hf_xet` never bypasses the mirror. Xet pass-through for very large files is a config flag.

## Error contract

All error headers are lower-case. Bodies are JSON when shown.

| Case | Status | `x-error-code` | Body |
| --- | --- | --- | --- |
| Repo unknown or denied | 401 | `RepoNotFound` | `{"error":"Repository not found"}` |
| Bad `repo_type` or unknown route | 404 | `RepoNotFound` | `{"error":"Sorry, we can't find the page you are looking for."}` |
| Path not in revision | 404 | `EntryNotFound` | empty |
| Bad revision | 404 | `RevisionNotFound` | `{"error":"Invalid rev id: <rev>"}` |
| Upstream unreachable or rate limited (408, 425, 429, 5xx) | 504 | `ProxyTimeout` | empty |
| Upstream returned garbage | 504 | `ProxyInvalidData` | empty |

401 for a missing repo is deliberate: `huggingface_hub` maps it to
`RepositoryNotFoundError`.

## Revision resolution

1. Index: if the repo is held locally, `main`, a tag or a sha resolves against the index
   (HF cache `refs/*`, git clone refs, or the single synthetic revision of a loose folder).
2. Upstream (online, proxy allowed): `HEAD /api/{type}/{ref}` proves visibility, then
   `GET /api/{type}/{ref}/revision/{rev}` yields `sha`. Results are cached in memory with a
   TTL and on disk under the metadata cache.
3. Offline: the newest cached `revision/*/meta.json` by `lastModified` is the tip.

## Access rules

Two rule lists, `proxy` and `cache`, each `{ repo, allow, use_re }`. Glob rules use
`globset` with `*` crossing `/` (olah `fnmatch` semantics); regex rules are anchored at
the start only. Last matching rule wins, no match means deny. Subject is `org/repo`
without repo type. A proxy denial is 401 `RepoNotFound`. A cache denial only disables
persistence.

## Block cache on disk

Under `<data-dir>/repos/`:

```
api/{repo_type}/{org_repo}/revision/{sha}/meta_get.json[.body]
api/{repo_type}/{org_repo}/tree/{sha}/{path}/tree_get_recursive_{b}_expand_{b}.json[.body]
api/{repo_type}/{org_repo}/paths-info/{sha}/{path}/paths-info_post.json[.body]
api/{repo_type}/{org_repo}/commits/{sha}/commits_get.json[.body]
files/{repo_type}/{org_repo}/resolve/{sha}/{file_path}/     block-cache entry
lfs/files/{d1}/{d2}/{hash_repo}/{hash_file}/                block-cache entry
lfs/object_index/{h[:2]}/{sha256}.json                      content hash -> repos
peers/{sha256-of-peer-base-url}/...                         the same tree, per peer
```

Those keys hold what the origin Hub served. A peer is another hoard node speaking the same
API, and it chooses both the bytes it returns and the hash it promises for them, so its copy
of a revision is keyed under `peers/{key}/` instead: a peer can never place bytes where a
client asking for `huggingface.co`'s copy would read them, nor in another peer's slot. The
key is the sha256 of the peer's base URL. Eviction sweeps from `repos/`, so every scope is
still subject to `cache_size_limit`.

A block-cache entry is a directory with `meta.bin` (magic `OLAH`, version 10, block size
64 MiB, chunk size 1 MiB, file size, etag), `chunks.crc` (little-endian u32 CRC32 per 1 MiB
chunk of the logical file) and `blocks/block_{n:08}.bin`. A block is present iff its file
exists with non-zero size; the presence bitmap is rebuilt from one directory scan on open
and never persisted. Missing blocks are fetched with a `Range` request by exactly one
downloader (advisory lock on `block_{n:08}.bin.dl.lock`), CRCs are written before the block
is renamed into place, and a CRC mismatch on read invalidates that block. Eviction runs
hourly when `cache_size_limit` is set, removing whole entries by LRU, FIFO or LARGE_FIRST.

Hoard keeps this layout so an existing olah cache directory can be pointed at directly.

## Deviations from olah

- One upstream request per proxied response, never two.
- paths-info is answered from the index for local repos and batched upstream otherwise.
- `..` in paths is rejected, not stripped.
- The 3-segment catch-all CDN route is registered last and only matches 40 or 64 hex
  characters in the final segment.
- Completed cache entries are ingested into the index, so a fully cached upstream file
  becomes a local artifact with a role and appears in search.
- Cache keys carry the upstream that served them: olah has one upstream, hoard also pulls
  from LAN peers, which are not the audience of the caller's token and are not trusted to
  supply the origin's bytes.
- ETag quoting is consistent: always quoted on resolve, LFS and xet responses.
- Routes hoard adds beyond olah: `GET /api/models`, `/api/datasets`, `/api/spaces` list
  endpoints (with `search`, `author`, `limit` query params) backed by the index,
  `GET /healthz`, the git smart HTTP face (`/{org}/{repo}[.git]/info/refs`,
  `git-upload-pack`, LFS batch) and the `/api/v1` hoard API.
- Every served response is appended to the usage log (time, client, path, bytes, rate).
