-- hoard index schema, version 1.
--
-- The filesystem stays authoritative: every row here is recoverable by rescanning disks
-- and re-gossiping peers. Identity is surrogate integers so a blob can exist before its
-- sha256 has been computed; content hashes are unique when present but never required.

PRAGMA foreign_keys = ON;

CREATE TABLE node (
    id            INTEGER PRIMARY KEY,
    node_id       TEXT    NOT NULL UNIQUE,
    name          TEXT    NOT NULL UNIQUE,
    url           TEXT,
    is_self       INTEGER NOT NULL DEFAULT 0,
    online        INTEGER NOT NULL DEFAULT 1,
    indexed_bytes INTEGER NOT NULL DEFAULT 0,
    last_seen     TEXT
);

CREATE TABLE source (
    id         INTEGER PRIMARY KEY,
    name       TEXT    NOT NULL UNIQUE,
    path       TEXT    NOT NULL,
    kind       TEXT    NOT NULL,
    node_id    INTEGER NOT NULL REFERENCES node(id) ON DELETE CASCADE,
    file_count INTEGER NOT NULL DEFAULT 0,
    blob_count INTEGER NOT NULL DEFAULT 0,
    byte_count INTEGER NOT NULL DEFAULT 0,
    last_scan  TEXT
);

-- Unique bytes. sha256 arrives later than the row: fingerprint is what scan time knows.
CREATE TABLE blob (
    id          INTEGER PRIMARY KEY,
    sha256      TEXT    UNIQUE,
    git_sha1    TEXT,
    fingerprint TEXT    NOT NULL,
    size        INTEGER NOT NULL,
    path_count  INTEGER NOT NULL DEFAULT 0,
    first_seen  TEXT    NOT NULL,
    hashed_at   TEXT
);

CREATE INDEX blob_fingerprint_size ON blob(fingerprint, size);
CREATE INDEX blob_unhashed ON blob(sha256) WHERE sha256 IS NULL;

-- A blob interpreted. One row per (blob, sniff version); the same bytes on twenty paths
-- stay one artifact, which is what makes a file appear on twenty shelves.
CREATE TABLE artifact (
    id              INTEGER PRIMARY KEY,
    blob_id         INTEGER NOT NULL REFERENCES blob(id) ON DELETE CASCADE,
    sniff_version   INTEGER NOT NULL DEFAULT 1,
    format          TEXT    NOT NULL DEFAULT 'unknown',
    role            TEXT    NOT NULL DEFAULT 'unk',
    sub_role        TEXT,
    role_confidence REAL    NOT NULL DEFAULT 0.0,
    base            TEXT    NOT NULL DEFAULT '',
    base_label      TEXT,
    base_confidence REAL    NOT NULL DEFAULT 0.0,
    precision       TEXT,
    quant           TEXT,
    rank            INTEGER,
    alpha           REAL,
    params          INTEGER,
    context         INTEGER,
    trigger_words   TEXT,
    org             TEXT,
    name            TEXT    NOT NULL DEFAULT '',
    triage          INTEGER NOT NULL DEFAULT 0,
    last_used       TEXT,
    use_count       INTEGER NOT NULL DEFAULT 0,
    created_at      TEXT    NOT NULL,
    UNIQUE(blob_id, sniff_version)
);

CREATE INDEX artifact_role ON artifact(role);
CREATE INDEX artifact_base ON artifact(base);
CREATE INDEX artifact_format ON artifact(format);
CREATE INDEX artifact_triage ON artifact(triage) WHERE triage = 1;

CREATE TABLE tensor_summary (
    artifact_id  INTEGER PRIMARY KEY REFERENCES artifact(id) ON DELETE CASCADE,
    tensor_count INTEGER NOT NULL DEFAULT 0,
    params       INTEGER NOT NULL DEFAULT 0,
    dtypes       TEXT    NOT NULL DEFAULT '{}',
    key_prefixes TEXT    NOT NULL DEFAULT '[]',
    samples      TEXT    NOT NULL DEFAULT '[]',
    metadata     TEXT    NOT NULL DEFAULT '{}'
);

-- Every inferred field records the fact that produced it, for the DERIVED FACTS panel.
CREATE TABLE evidence (
    id          INTEGER PRIMARY KEY,
    artifact_id INTEGER NOT NULL REFERENCES artifact(id) ON DELETE CASCADE,
    field       TEXT    NOT NULL,
    value       TEXT    NOT NULL,
    source      TEXT    NOT NULL,
    detail      TEXT,
    confidence  REAL    NOT NULL DEFAULT 0.0
);

CREATE INDEX evidence_artifact ON evidence(artifact_id);

-- A user decision that outranks inference and survives rescans.
CREATE TABLE artifact_override (
    artifact_id INTEGER PRIMARY KEY REFERENCES artifact(id) ON DELETE CASCADE,
    role        TEXT,
    sub_role    TEXT,
    base        TEXT,
    decided_at  TEXT NOT NULL
);

-- Paths a rejection marked as noise; later scans skip them.
CREATE TABLE noise (
    id         INTEGER PRIMARY KEY,
    path       TEXT NOT NULL UNIQUE,
    reason     TEXT,
    decided_at TEXT NOT NULL
);

CREATE TABLE repo (
    id            INTEGER PRIMARY KEY,
    qualified     TEXT    NOT NULL,
    repo_type     TEXT    NOT NULL,
    org           TEXT,
    name          TEXT    NOT NULL,
    node_id       INTEGER NOT NULL REFERENCES node(id) ON DELETE CASCADE,
    source_id     INTEGER REFERENCES source(id) ON DELETE SET NULL,
    kind          TEXT    NOT NULL,
    root_path     TEXT,
    git_dir       TEXT,
    default_ref   TEXT    NOT NULL DEFAULT 'main',
    description   TEXT,
    card_data     TEXT,
    license       TEXT,
    downloads     INTEGER NOT NULL DEFAULT 0,
    likes         INTEGER NOT NULL DEFAULT 0,
    private       INTEGER NOT NULL DEFAULT 0,
    gated         INTEGER NOT NULL DEFAULT 0,
    last_modified TEXT,
    created_at    TEXT,
    UNIQUE(qualified, node_id)
);

CREATE INDEX repo_lookup ON repo(qualified);

CREATE TABLE revision (
    id        INTEGER PRIMARY KEY,
    repo_id   INTEGER NOT NULL REFERENCES repo(id) ON DELETE CASCADE,
    sha       TEXT    NOT NULL,
    title     TEXT,
    author    TEXT,
    committed TEXT,
    synthetic INTEGER NOT NULL DEFAULT 0,
    UNIQUE(repo_id, sha)
);

CREATE TABLE git_ref (
    id          INTEGER PRIMARY KEY,
    repo_id     INTEGER NOT NULL REFERENCES repo(id) ON DELETE CASCADE,
    name        TEXT    NOT NULL,
    kind        TEXT    NOT NULL,
    revision_id INTEGER NOT NULL REFERENCES revision(id) ON DELETE CASCADE,
    UNIQUE(repo_id, name)
);

-- A path inside a revision pointing at a blob, on one node.
CREATE TABLE entry (
    id            INTEGER PRIMARY KEY,
    revision_id   INTEGER NOT NULL REFERENCES revision(id) ON DELETE CASCADE,
    path          TEXT    NOT NULL,
    blob_id       INTEGER REFERENCES blob(id) ON DELETE SET NULL,
    artifact_id   INTEGER REFERENCES artifact(id) ON DELETE SET NULL,
    node_id       INTEGER NOT NULL REFERENCES node(id) ON DELETE CASCADE,
    source_id     INTEGER REFERENCES source(id) ON DELETE SET NULL,
    abs_path      TEXT,
    size          INTEGER NOT NULL DEFAULT 0,
    is_symlink    INTEGER NOT NULL DEFAULT 0,
    lfs_pointer   INTEGER NOT NULL DEFAULT 0,
    state         TEXT    NOT NULL DEFAULT 'local',
    pull_progress REAL,
    present       INTEGER NOT NULL DEFAULT 1,
    mtime         INTEGER,
    inode         INTEGER,
    last_seen     TEXT    NOT NULL,
    UNIQUE(revision_id, path, node_id)
);

CREATE INDEX entry_artifact ON entry(artifact_id);
CREATE INDEX entry_blob ON entry(blob_id);
CREATE INDEX entry_revision ON entry(revision_id);
CREATE INDEX entry_abs_path ON entry(abs_path);

CREATE TABLE shelf (
    id       INTEGER PRIMARY KEY,
    name     TEXT    NOT NULL UNIQUE,
    query    TEXT    NOT NULL,
    builtin  INTEGER NOT NULL DEFAULT 0,
    position INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE loadout (
    id         INTEGER PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    document   TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE job (
    id         INTEGER PRIMARY KEY,
    kind       TEXT    NOT NULL,
    state      TEXT    NOT NULL DEFAULT 'running',
    phase      TEXT,
    subject    TEXT,
    progress   REAL    NOT NULL DEFAULT 0.0,
    bytes      INTEGER NOT NULL DEFAULT 0,
    total      INTEGER,
    rate_bps   INTEGER,
    error      TEXT,
    started    TEXT    NOT NULL,
    finished   TEXT
);

CREATE INDEX job_state ON job(state, id DESC);

-- Every served request, which feeds the USAGE tab, `used:` queries and `pulls on lan`.
CREATE TABLE usage (
    id          INTEGER PRIMARY KEY,
    at          TEXT    NOT NULL,
    client      TEXT,
    repo_id     INTEGER REFERENCES repo(id) ON DELETE SET NULL,
    artifact_id INTEGER REFERENCES artifact(id) ON DELETE SET NULL,
    path        TEXT    NOT NULL,
    bytes       INTEGER NOT NULL DEFAULT 0,
    rate_bps    INTEGER,
    status      INTEGER NOT NULL DEFAULT 200
);

CREATE INDEX usage_at ON usage(at DESC);
CREATE INDEX usage_artifact ON usage(artifact_id, at DESC);
CREATE INDEX usage_repo ON usage(repo_id, at DESC);

CREATE TABLE setting (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- Symlinks written by `hoard link` and `hoard apply`, so PATHS ON DISK can name them.
CREATE TABLE projection (
    id          INTEGER PRIMARY KEY,
    link_path   TEXT    NOT NULL UNIQUE,
    target_path TEXT    NOT NULL,
    profile     TEXT    NOT NULL,
    artifact_id INTEGER REFERENCES artifact(id) ON DELETE CASCADE,
    written_at  TEXT    NOT NULL
);

-- Gossip bookkeeping: how far this node has read each peer's digest.
CREATE TABLE peer_cursor (
    node_id  INTEGER PRIMARY KEY REFERENCES node(id) ON DELETE CASCADE,
    cursor   TEXT,
    synced   TEXT
);

CREATE VIRTUAL TABLE artifact_fts USING fts5(
    artifact_id UNINDEXED,
    name,
    org,
    base,
    trigger_words,
    keys,
    card,
    tokenize = 'unicode61 remove_diacritics 2'
);

-- One row per entry, flattened to exactly the columns the search grammar filters on and
-- the API row shape returns. `preference` ranks which entry represents an artifact when
-- the same bytes exist on several nodes: this node first, then bytes present locally.
CREATE VIEW search_row AS
SELECT
    a.id                                          AS artifact_id,
    a.blob_id                                     AS blob_id,
    b.sha256                                      AS blob_sha256,
    b.git_sha1                                    AS blob_git_sha1,
    a.role                                        AS role,
    a.sub_role                                    AS sub_role,
    a.role_confidence                             AS role_confidence,
    a.org                                         AS org,
    a.name                                        AS name,
    a.base                                        AS base,
    a.base_label                                  AS base_label,
    a.format                                      AS format,
    a.precision                                   AS precision,
    a.quant                                       AS quant,
    a.rank                                        AS rank,
    a.alpha                                       AS alpha,
    a.params                                      AS params,
    a.context                                     AS context,
    a.trigger_words                               AS trigger_words,
    a.triage                                      AS triage,
    a.last_used                                   AS last_used,
    b.size                                        AS size,
    b.path_count                                  AS path_count,
    CASE WHEN b.path_count > 1 THEN 1 ELSE 0 END  AS dupe,
    e.id                                          AS entry_id,
    e.path                                        AS path,
    e.abs_path                                    AS abs_path,
    e.state                                       AS state,
    e.pull_progress                               AS pull_progress,
    e.is_symlink                                  AS is_symlink,
    e.lfs_pointer                                 AS lfs_pointer,
    e.present                                     AS present,
    e.mtime                                       AS mtime,
    r.id                                          AS repo_id,
    r.qualified                                   AS repo,
    r.repo_type                                   AS repo_type,
    r.name                                        AS repo_name,
    r.org                                         AS repo_org,
    v.sha                                         AS revision_sha,
    COALESCE(
        (SELECT g.name FROM git_ref g WHERE g.revision_id = v.id ORDER BY
            CASE g.kind WHEN 'branch' THEN 0 ELSE 1 END, g.name LIMIT 1),
        substr(v.sha, 1, 7)
    )                                             AS revision,
    n.id                                          AS node_row_id,
    n.name                                        AS node,
    n.is_self                                     AS node_is_self,
    s.name                                        AS source,
    (n.is_self * 4)
      + CASE e.state WHEN 'served' THEN 3 WHEN 'local' THEN 2 WHEN 'triage' THEN 2
                     WHEN 'stale' THEN 1 ELSE 0 END
      + e.present                                 AS preference
FROM artifact a
JOIN blob b     ON b.id = a.blob_id
JOIN entry e    ON e.artifact_id = a.id
JOIN revision v ON v.id = e.revision_id
JOIN repo r     ON r.id = v.repo_id
JOIN node n     ON n.id = e.node_id
LEFT JOIN source s ON s.id = e.source_id;
