use std::collections::HashMap;

use hoard_core::CommitSha;

use crate::catalog::models::{Ref, Repository, Revision, repository_key};
use crate::discover::{DiscoveredRepo, RevisionName};

/// The stored shape of a discovered repository. `present` is what the copy already on
/// record counted, because rediscovering a repository does not move a single file, and
/// `revisions` is what [`merged_revisions`] settled, not what discovery listed.
pub(super) fn repository(
    id: i64,
    source: &str,
    repo: &DiscoveredRepo,
    present: (u32, u64),
    revisions: Vec<Revision>,
) -> Repository {
    Repository {
        id,
        present_files: present.0,
        present_bytes: present.1,
        key: repository_key(source, &repo.repo),
        source: source.to_owned(),
        repo: repo.repo.clone(),
        root_path: repo.root_path.to_string_lossy().into_owned(),
        git_dir: repo
            .git_dir
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
        default_ref: repo.default_ref.clone(),
        remote: repo.remote.clone(),
        revisions,
    }
}

/// What a revision is matched by across scans: its commit, or its name when it has none.
#[derive(PartialEq, Eq, Hash)]
enum RevisionKey {
    Commit(CommitSha),
    Named(String),
}

fn revision_key(sha: Option<CommitSha>, name: &str) -> RevisionKey {
    match sha {
        Some(sha) => RevisionKey::Commit(sha),
        None => RevisionKey::Named(name.to_owned()),
    }
}

/// The shape discovery just gave one revision of a repository, before it is matched against
/// what the catalog already holds.
fn discovered_revision(repo: &DiscoveredRepo, position: usize) -> Revision {
    let revision = &repo.revisions[position];
    Revision {
        name: revision_name(repo, position),
        sha: match revision.name {
            RevisionName::Commit(sha) => Some(sha),
            RevisionName::Synthetic => None,
        },
        title: revision.title.clone(),
        author: revision.author.clone(),
        committed: revision.committed.map(|stamp| stamp.to_string()),
        refs: repo
            .refs
            .iter()
            .filter(|reference| reference.revision == position)
            .map(|reference| Ref {
                name: reference.name.clone(),
                kind: reference.kind,
            })
            .collect(),
        retired: false,
    }
}

/// The stored revisions of a repository merged with what discovery found now, and the
/// ordinal each discovered revision takes in the merged list.
///
/// An entry names its revision by ordinal, so an ordinal is an identity and cannot follow
/// discovery order: a snapshot whose commit sorts ahead of the stored ones would otherwise
/// move every entry of the repository onto the wrong revision. A rediscovered revision
/// keeps its slot and takes the fresh refs and metadata; a new one is appended; one the
/// walk no longer found keeps its slot so the entries filed under it still resolve, and
/// gives up its refs, because a ref is a pointer discovery just re-enumerated and a
/// revision no ref reaches must not answer to a name that now belongs to another.
pub(super) fn merged_revisions(
    stored: &[Revision],
    repo: &DiscoveredRepo,
) -> (Vec<Revision>, Vec<usize>) {
    let mut slots: HashMap<RevisionKey, usize> = HashMap::with_capacity(stored.len());
    for (slot, revision) in stored.iter().enumerate() {
        slots
            .entry(revision_key(revision.sha, &revision.name))
            .or_insert(slot);
    }
    let mut merged: Vec<Revision> = stored
        .iter()
        .map(|revision| Revision {
            refs: Vec::new(),
            ..revision.clone()
        })
        .collect();
    let mut ordinals = Vec::with_capacity(repo.revisions.len());
    for position in 0..repo.revisions.len() {
        let found = discovered_revision(repo, position);
        let slot = match slots.remove(&revision_key(found.sha, &found.name)) {
            Some(slot) => {
                merged[slot] = found;
                slot
            }
            None => {
                merged.push(found);
                merged.len() - 1
            }
        };
        ordinals.push(slot);
    }
    (merged, ordinals)
}

fn revision_name(repo: &DiscoveredRepo, ordinal: usize) -> String {
    repo.refs
        .iter()
        .filter(|reference| reference.revision == ordinal)
        .min_by_key(|reference| {
            (
                reference.kind != crate::discover::RefKind::Branch,
                &reference.name,
            )
        })
        .map(|reference| reference.name.clone())
        .unwrap_or_else(|| match repo.revisions[ordinal].name {
            RevisionName::Commit(sha) => sha.to_string().chars().take(7).collect(),
            RevisionName::Synthetic => repo.default_ref.clone(),
        })
}
