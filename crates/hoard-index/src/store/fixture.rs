use jiff::Timestamp;
use rusqlite::Connection;

use hoard_core::{
    ArtifactFormat, ArtifactId, Base, BlobId, CommitSha, NodeId, Precision, RepoId, RepoRowId,
    RepoType, RevisionId, Role, Sha256, SourceId, State,
};

use crate::config::SourceKind;
use crate::store::artifact::{NewArtifact, upsert_artifact};
use crate::store::blob::{set_sha256, upsert_blob};
use crate::store::entry::{NewEntry, upsert_entry};
use crate::store::node::{NewNode, upsert_node};
use crate::store::repo::{NewRepo, upsert_repo};
use crate::store::revision::{NewRevision, RefKind, upsert_ref, upsert_revision};
use crate::store::source::upsert_source;

pub(crate) fn at() -> Timestamp {
    "2026-09-04T09:10:00Z".parse().expect("timestamp parses")
}

/// A synthetic repository with two revisions, a duplicated blob and its artifacts.
pub(crate) struct Fixture {
    pub node_row: i64,
    pub source: SourceId,
    pub repo: RepoRowId,
    pub first_revision: RevisionId,
    pub second_revision: RevisionId,
    pub shared_blob: BlobId,
    pub other_blob: BlobId,
    pub shared_artifact: ArtifactId,
    pub other_artifact: ArtifactId,
}

/// The same artifact as a peer node reports it.
pub(crate) struct PeerCopy {
    pub node_row: i64,
    pub repo: RepoRowId,
    pub revision: RevisionId,
}

pub(crate) const SHARED_SIZE: u64 = 239_075_328;

impl Fixture {
    pub(crate) fn build(conn: &Connection) -> Fixture {
        let node_row = upsert_node(
            conn,
            &NewNode::local(
                NodeId::new("workstation-1").expect("node id"),
                "workstation",
            ),
        )
        .expect("node inserts");
        let source = upsert_source(
            conn,
            "cache",
            std::path::Path::new("/data/hub"),
            SourceKind::HfCache,
            node_row,
        )
        .expect("source inserts");
        let repo = upsert_repo(
            conn,
            &NewRepo::new(
                RepoId::parse_ref(RepoType::Models, "org/name").expect("reference parses"),
                node_row,
                SourceKind::HfCache,
            ),
        )
        .expect("repo inserts");

        let first_revision = upsert_revision(
            conn,
            &NewRevision::new(repo, CommitSha::from_bytes([0x46; 20])),
        )
        .expect("revision inserts");
        let second_revision = upsert_revision(
            conn,
            &NewRevision::new(repo, CommitSha::from_bytes([0x91; 20])),
        )
        .expect("revision inserts");
        upsert_ref(conn, repo, "main", RefKind::Branch, first_revision).expect("branch inserts");
        upsert_ref(conn, repo, "v1.0", RefKind::Tag, second_revision).expect("tag inserts");

        let shared_blob =
            upsert_blob(conn, "xxh3:shared", SHARED_SIZE, at()).expect("blob inserts");
        set_sha256(
            conn,
            shared_blob,
            Sha256::from_bytes([0x9f; 32]),
            None,
            at(),
        )
        .expect("hash records");
        let other_blob = upsert_blob(conn, "xxh3:other", 1024, at()).expect("blob inserts");

        let shared_artifact = upsert_artifact(
            conn,
            &NewArtifact {
                format: ArtifactFormat::Safetensors,
                role: Role::Lora,
                role_confidence: 0.98,
                base: Base::new("sdxl 1.0"),
                base_label: Some("sdxl 1.0".to_owned()),
                base_confidence: 0.9,
                precision: Some(Precision::new("fp16")),
                rank: Some(32),
                trigger: Some("film grain, 35mm".to_owned()),
                org: Some("kohya".to_owned()),
                ..NewArtifact::new(shared_blob, "film-grain-35mm")
            },
            at(),
        )
        .expect("artifact inserts");
        let other_artifact = upsert_artifact(
            conn,
            &NewArtifact {
                format: ArtifactFormat::Config,
                ..NewArtifact::new(other_blob, "config")
            },
            at(),
        )
        .expect("artifact inserts");

        let weights = |revision: RevisionId, snapshot: &str| NewEntry {
            blob: Some(shared_blob),
            artifact: Some(shared_artifact),
            source: Some(source),
            abs_path: Some(format!(
                "/data/hub/snapshots/{snapshot}/unet/model.safetensors"
            )),
            size: SHARED_SIZE,
            state: State::Served,
            ..NewEntry::new(revision, "unet/model.safetensors", node_row)
        };
        upsert_entry(conn, &weights(first_revision, "aaa"), at()).expect("entry inserts");
        upsert_entry(conn, &weights(second_revision, "bbb"), at()).expect("entry inserts");
        upsert_entry(
            conn,
            &NewEntry {
                blob: Some(other_blob),
                artifact: Some(other_artifact),
                source: Some(source),
                abs_path: Some("/data/hub/snapshots/aaa/config.json".to_owned()),
                size: 1024,
                ..NewEntry::new(first_revision, "config.json", node_row)
            },
            at(),
        )
        .expect("entry inserts");

        Fixture {
            node_row,
            source,
            repo,
            first_revision,
            second_revision,
            shared_blob,
            other_blob,
            shared_artifact,
            other_artifact,
        }
    }

    /// Adds a peer node reporting the same artifact, which is what makes a search row
    /// ambiguous until preference decides.
    pub(crate) fn add_peer_copy(&self, conn: &Connection) -> PeerCopy {
        let node_row = upsert_node(
            conn,
            &NewNode::peer(
                NodeId::new("laptop-1").expect("node id"),
                "laptop",
                "http://laptop:7099",
            ),
        )
        .expect("peer inserts");
        let repo = upsert_repo(
            conn,
            &NewRepo::new(
                RepoId::parse_ref(RepoType::Models, "org/name").expect("reference parses"),
                node_row,
                SourceKind::Git,
            ),
        )
        .expect("repo inserts");
        let revision = upsert_revision(
            conn,
            &NewRevision::new(repo, CommitSha::from_bytes([0x46; 20])),
        )
        .expect("revision inserts");
        upsert_entry(
            conn,
            &NewEntry {
                blob: Some(self.shared_blob),
                artifact: Some(self.shared_artifact),
                size: SHARED_SIZE,
                state: State::Remote,
                ..NewEntry::new(revision, "loras/film-grain-35mm.safetensors", node_row)
            },
            at(),
        )
        .expect("entry inserts");
        PeerCopy {
            node_row,
            repo,
            revision,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::DiskPressure;
    use crate::store::Store;
    use crate::store::blob::{blob, disk_pressure};
    use crate::store::entry::{entries_for_blob, entries_for_revision};
    use crate::store::revision::{resolve_revision, revisions_for_repo};
    use crate::store::search_exec::query_rows;

    #[test]
    fn the_synthetic_repository_round_trips_with_its_duplicate_blob() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");
        let world = Fixture::build(&conn);

        assert_eq!(
            revisions_for_repo(&conn, world.repo)
                .expect("revisions list")
                .len(),
            2
        );
        assert_eq!(
            resolve_revision(&conn, world.repo, "main")
                .expect("branch resolves")
                .id,
            world.first_revision
        );
        assert_eq!(
            resolve_revision(&conn, world.repo, "v1.0")
                .expect("tag resolves")
                .id,
            world.second_revision
        );
        assert_eq!(
            entries_for_revision(&conn, world.first_revision)
                .expect("list")
                .len(),
            2
        );
        assert_eq!(
            entries_for_revision(&conn, world.second_revision)
                .expect("list")
                .len(),
            1
        );

        assert_eq!(
            blob(&conn, world.shared_blob)
                .expect("blob reads")
                .path_count,
            2
        );
        assert_eq!(
            blob(&conn, world.other_blob)
                .expect("blob reads")
                .path_count,
            1
        );
        assert_eq!(
            entries_for_blob(&conn, world.shared_blob)
                .expect("list")
                .len(),
            2
        );
        assert_eq!(
            disk_pressure(&conn).expect("pressure"),
            DiskPressure {
                reclaimable_bytes: SHARED_SIZE,
                duplicate_blobs: 1
            }
        );

        let rows = query_rows(&conn, "", &[], "", 50, 0).expect("rows read");
        let shared = rows
            .iter()
            .find(|row| row.artifact_id == world.shared_artifact)
            .expect("the duplicated artifact is listed");
        assert!(shared.dupe);
        assert!(shared.blob_sha256.is_some());
        let other = rows
            .iter()
            .find(|row| row.artifact_id == world.other_artifact)
            .expect("the config artifact is listed");
        assert!(!other.dupe);
    }

    #[test]
    fn a_peer_copy_adds_a_node_without_duplicating_the_artifact() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");
        let world = Fixture::build(&conn);
        let peer = world.add_peer_copy(&conn);

        assert_ne!(peer.node_row, world.node_row);
        assert_ne!(peer.repo, world.repo);
        assert_ne!(peer.revision, world.first_revision);
        assert_eq!(
            entries_for_blob(&conn, world.shared_blob)
                .expect("list")
                .len(),
            3
        );
        assert_eq!(
            query_rows(&conn, "", &[], "", 50, 0)
                .expect("rows read")
                .len(),
            2
        );
    }
}
