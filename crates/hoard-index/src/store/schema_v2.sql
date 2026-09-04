-- hoard index schema, version 2: one artifact per model, however many files it was split
-- across.
--
-- A shard set is one artifact anchored on its lowest numbered shard, and every shard's
-- entry points at that artifact. The blob stays per file, so `group_bytes` records what the
-- whole model weighs and `shard_count` how many files carry it. Both are null for a model
-- that is a single file, which is what the view falls back to.

ALTER TABLE artifact ADD COLUMN group_bytes INTEGER;
ALTER TABLE artifact ADD COLUMN shard_count INTEGER;

DROP VIEW search_row;

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
    COALESCE(a.group_bytes, b.size)               AS size,
    COALESCE(a.shard_count, 1)                    AS shard_count,
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
    ((n.is_self * 4)
      + CASE e.state WHEN 'served' THEN 3 WHEN 'local' THEN 2 WHEN 'triage' THEN 2
                     WHEN 'stale' THEN 1 ELSE 0 END
      + e.present) * 2
      + CASE WHEN e.blob_id = a.blob_id THEN 1 ELSE 0 END
                                                  AS preference
FROM artifact a
JOIN blob b     ON b.id = a.blob_id
JOIN entry e    ON e.artifact_id = a.id
JOIN revision v ON v.id = e.revision_id
JOIN repo r     ON r.id = v.repo_id
JOIN node n     ON n.id = e.node_id
LEFT JOIN source s ON s.id = e.source_id;
