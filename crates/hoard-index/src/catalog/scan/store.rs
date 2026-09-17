use std::cmp::Reverse;

use native_db::transaction::RwTransaction;

use hoard_core::{Artifact, ArtifactFormat, ArtifactId, BlobId, Sha256, State, TensorSummary};

use crate::catalog::hashing::content_key;
use crate::catalog::models::{
    ArtifactDetail, ArtifactRecord, ArtifactRecordKey, Entry, LOCAL_NODE, Override, Placement,
    Repository, SearchText, Summary,
};
use crate::catalog::verdict::apply_override;
use crate::config::{HashPolicy, IndexConfig, SourceKind};
use crate::discover::{DiscoveredRepo, DiscoveredRevision};
use crate::error::Result;
use crate::infer::Inference;

use super::counters::{Counters, blank_summary, next_id};
use super::plan::{ArtifactDraft, EntryRole, PreparedEntry};
use super::revisions::Fingerprints;

pub(super) struct EntryContext<'a> {
    pub(super) repo: &'a DiscoveredRepo,
    pub(super) repo_id: i64,
    /// What the pass settled for this repository's paths before the walk, so an entry is
    /// written with the fingerprint its revision's sha was derived from.
    pub(super) fingerprints: &'a Fingerprints,
    pub(super) revision: &'a DiscoveredRevision,
    pub(super) settings: ScanSettings,
    pub(super) kind: SourceKind,
    pub(super) ordinal: u32,
}

/// What one walk was asked for beyond the roots themselves: where inference stops being
/// trusted, and whether the files it reads are hashed as it goes.
#[derive(Debug, Clone, Copy)]
pub(super) struct ScanSettings {
    pub(super) triage: f32,
    pub(super) policy: HashPolicy,
    pub(super) threshold: u64,
}

impl ScanSettings {
    pub(super) fn new(config: &IndexConfig, policy: HashPolicy) -> Self {
        Self {
            triage: config.triage_confidence,
            policy,
            threshold: config.lfs_threshold,
        }
    }
}

/// Writes one unit. A shard member carries no artifact of its own, so it is stored after
/// the anchor that establishes the artifact and the triage verdict it shares.
pub(super) fn store_unit(
    transaction: &RwTransaction<'_>,
    context: &EntryContext<'_>,
    prepared: Vec<PreparedEntry<'_>>,
    counters: &mut Counters,
) -> Result<()> {
    let mut anchor = None;
    let mut members = Vec::new();
    for prepared in prepared {
        match &prepared.role {
            EntryRole::Member => members.push(prepared),
            EntryRole::Anchor(_) => {
                anchor = store_entry(transaction, context, prepared, None, counters)?;
            }
            EntryRole::Standalone => {
                store_entry(transaction, context, prepared, None, counters)?;
            }
        }
    }
    for prepared in members {
        store_entry(transaction, context, prepared, anchor, counters)?;
    }
    Ok(())
}

/// Writes one file. A shard member carries no artifact of its own: it is handed the
/// artifact and the triage verdict its anchor already established.
fn store_entry(
    transaction: &RwTransaction<'_>,
    context: &EntryContext<'_>,
    prepared: PreparedEntry<'_>,
    anchor: Option<(i64, bool)>,
    counters: &mut Counters,
) -> Result<Option<(i64, bool)>> {
    let entry_id = next_id(&mut counters.entries)?;
    let record = match prepared.artifact.as_ref() {
        Some(draft) => Some(store_artifact(transaction, &prepared, draft, counters)?),
        None => None,
    };
    let stored = match &record {
        Some(record) => Some((record.id, record.needs_triage(context.settings.triage))),
        None => anchor,
    };
    let triage = stored.is_some_and(|(_, triage)| triage);
    let state = entry_state(&prepared, triage);
    let artifact_id = stored.map(|(id, _)| id);
    let entry = Entry {
        id: entry_id,
        repo_id: context.repo_id,
        artifact_id,
        revision: context.ordinal,
        rel_path: prepared.discovered.rel_path.clone(),
        placed: Placement::of(
            prepared
                .presence
                .abs_path
                .as_ref()
                .map(|path| path.to_string_lossy())
                .as_deref(),
            &context.repo.root_path.to_string_lossy(),
            &prepared.discovered.rel_path,
        ),
        size: prepared.presence.size,
        state,
        triage,
        preference: preference(state, prepared.presence.present()),
        lfs_pointer: prepared.presence.lfs_pointer,
        is_symlink: prepared.discovered.is_symlink,
        sha256: prepared.presence.sha256,
        content: prepared.presence.sha256.as_ref().map(content_key),
        git_sha1: prepared.presence.git_sha1,
        mtime: prepared.discovered.stamp.map(|stamp| stamp.mtime),
        inode: prepared.discovered.stamp.map(|stamp| stamp.inode),
        fingerprint: context
            .fingerprints
            .get(&prepared.discovered.rel_path)
            .cloned(),
        node: LOCAL_NODE,
        pass: 0,
        pull_progress: None,
    };
    counters.derived.added(&entry);
    transaction.insert(entry)?;
    Ok(stored)
}

/// Which of two namings of one identity the record keeps.
///
/// The unit holding more of the content names it, because a shard set describes a model its
/// anchor's file name does not; at equal content a naming that spells the repository beats
/// one that spells a path inside it, because the repository is what a reader looked the
/// model up by; what is left is settled by the smaller name, so that two revisions naming
/// one file differently agree whichever of them the walk reaches first.
fn names_better(candidate: &ArtifactRecord, stored: &ArtifactRecord) -> bool {
    naming_key(candidate) < naming_key(stored)
}

fn naming_key(record: &ArtifactRecord) -> (Reverse<u64>, bool, &str, &str) {
    (
        Reverse(record.group_bytes.unwrap_or(record.artifact.size)),
        !record.named_by_repo,
        record.name.as_str(),
        record.org.as_deref().unwrap_or_default(),
    )
}

/// The record this entry contributes a path to.
///
/// A first path writes the record; an identity the catalog already holds keeps its id, when
/// it was first seen and its cleared verdict, takes what this file's header just proved, and
/// keeps the better of the two names by [`names_better`] rather than the first one written.
/// The identity is a unique secondary key, and a read inside the writing transaction sees
/// the rows that transaction has already written, so two copies of one file in one unit
/// reach the same record without a cache of what this pass has written.
fn store_artifact(
    transaction: &RwTransaction<'_>,
    prepared: &PreparedEntry<'_>,
    draft: &ArtifactDraft,
    counters: &mut Counters,
) -> Result<ArtifactRecord> {
    let stored: Option<ArtifactRecord> = transaction
        .get()
        .secondary(ArtifactRecordKey::identity, draft.identity)?;
    let id = match &stored {
        Some(stored) => stored.id,
        None => next_id(&mut counters.artifacts)?,
    };
    counters.touch(transaction, id)?;
    let (mut record, detail) = artifact_record(
        id,
        prepared,
        draft,
        stored.as_ref().map(|stored| stored.first_seen.clone()),
    );
    if let Some(stored) = &stored {
        record.triage_cleared = stored.triage_cleared;
        record.rejected = stored.rejected;
        if !names_better(&record, stored) {
            record.name = stored.name.clone();
            record.org = stored.org.clone();
        }
    }
    let verdict: Option<Override> = transaction.get().primary(id)?;
    if let Some(verdict) = &verdict {
        apply_override(&mut record, verdict);
    }
    let text = SearchText {
        artifact_id: id,
        keys: tensor_keys(detail.tensors.as_ref()),
    };
    match stored {
        Some(_) => {
            transaction.upsert(text)?;
            transaction.upsert(detail)?;
            transaction.upsert(record.clone())?;
        }
        None => {
            transaction.insert(text)?;
            transaction.insert(detail)?;
            transaction.insert(record.clone())?;
        }
    }
    counters.pending.push(id);
    Ok(record)
}

pub(in crate::catalog) fn tensor_keys(tensors: Option<&TensorSummary>) -> String {
    tensors.map_or_else(String::new, |summary| {
        summary
            .key_prefixes
            .iter()
            .map(|(key, _)| key.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    })
}

fn entry_state(prepared: &PreparedEntry<'_>, triage: bool) -> State {
    match (prepared.presence.present(), triage) {
        (false, _) => State::Remote,
        (true, true) => State::Triage,
        (true, false) => State::Local,
    }
}

/// How an entry ranks when several hold the same artifact: served bytes outrank local ones,
/// and a path this disk holds outranks one it does not.
pub(in crate::catalog) fn preference(state: State, present: bool) -> i64 {
    let weight = match state {
        State::Served => 3,
        State::Local | State::Triage => 2,
        State::Stale => 1,
        State::Pulling | State::Remote => 0,
    };
    (4 + weight + i64::from(present)) * 2 + 1
}

/// How one path of an artifact ranks against another when a reader keeps one of them: the
/// higher availability preference, and on a tie the lower id. The read API, the search
/// index's `primary` mark and the query path's fold all keep the same path because they all
/// order by this.
pub(in crate::catalog) fn collapse_key(preference: i64, id: i64) -> (i64, Reverse<i64>) {
    (preference, Reverse(id))
}

/// The one entry a collapse keeps out of the entries of a single artifact.
pub(in crate::catalog) fn preferred_entry(
    entries: impl IntoIterator<Item = Entry>,
) -> Option<Entry> {
    entries
        .into_iter()
        .max_by_key(|entry| collapse_key(entry.preference, entry.id))
}

/// One artifact as the catalog is about to record it, whether a scan read its header or the
/// mirror only knows its name, size and hash.
pub(in crate::catalog) struct RecordFacts<'a> {
    pub(in crate::catalog) id: i64,
    pub(in crate::catalog) identity: u128,
    pub(in crate::catalog) name: String,
    pub(in crate::catalog) org: Option<String>,
    pub(in crate::catalog) sha256: Option<Sha256>,
    pub(in crate::catalog) size: u64,
    pub(in crate::catalog) format: ArtifactFormat,
    pub(in crate::catalog) inference: &'a Inference,
    pub(in crate::catalog) tensors: Option<TensorSummary>,
    pub(in crate::catalog) group_bytes: Option<u64>,
    pub(in crate::catalog) shard_count: Option<u32>,
    pub(in crate::catalog) incomplete: bool,
    pub(in crate::catalog) named_by_repo: bool,
    /// When the catalog first recorded this artifact. A record being rewritten passes what
    /// it already carries, so nothing reads the clock for a row that has been seen before.
    pub(in crate::catalog) first_seen: Option<String>,
}

fn artifact_record(
    id: i64,
    prepared: &PreparedEntry<'_>,
    draft: &ArtifactDraft,
    first_seen: Option<String>,
) -> (ArtifactRecord, ArtifactDetail) {
    let (tensors, group_bytes, shard_count, incomplete) = match &prepared.role {
        EntryRole::Anchor(facts) => (
            facts.tensors.clone(),
            Some(facts.bytes),
            Some(facts.shard_count),
            facts.incomplete,
        ),
        EntryRole::Standalone | EntryRole::Member => (
            prepared
                .sniffed
                .as_ref()
                .and_then(|sniffed| sniffed.tensors.clone()),
            None,
            None,
            false,
        ),
    };
    record_of(RecordFacts {
        id,
        identity: draft.identity,
        name: prepared.name.clone(),
        org: prepared.org.clone(),
        sha256: prepared.presence.sha256,
        size: prepared.presence.size,
        format: prepared
            .sniffed
            .as_ref()
            .map_or(ArtifactFormat::Unknown, |sniffed| sniffed.format),
        inference: &draft.inference,
        tensors,
        group_bytes,
        shard_count,
        incomplete,
        named_by_repo: prepared.named_by_repo,
        first_seen,
    })
}

pub(in crate::catalog) fn record_of(facts: RecordFacts<'_>) -> (ArtifactRecord, ArtifactDetail) {
    let RecordFacts {
        id,
        identity,
        name,
        org,
        sha256,
        size,
        format,
        inference,
        tensors,
        group_bytes,
        shard_count,
        incomplete,
        named_by_repo,
        first_seen,
    } = facts;
    let record = ArtifactRecord {
        id,
        identity,
        name,
        org,
        path_count: 1,
        copies: 1,
        artifact: Artifact {
            id: ArtifactId::new(id),
            blob: BlobId::new(id),
            sha256,
            size,
            format,
            role: inference.role,
            sub_role: inference.sub_role,
            role_confidence: inference.role_confidence,
            base: inference.base.clone(),
            base_label: inference.base_label.clone(),
            base_confidence: inference.base_confidence,
            precision: inference.precision.clone(),
            quant: inference.quant.clone(),
            rank: inference.rank,
            alpha: inference.alpha,
            params: inference.params,
            context: inference.context,
            trigger: inference.trigger.clone(),
        },
        group_bytes,
        shard_count,
        incomplete,
        named_by_repo,
        triage_cleared: false,
        rejected: false,
        first_seen: first_seen.unwrap_or_else(|| jiff::Timestamp::now().to_string()),
    };
    let detail = ArtifactDetail::new(id, tensors, inference.evidence.clone());
    (record, detail)
}

pub(in crate::catalog) fn settle_repositories(transaction: &RwTransaction<'_>) -> Result<()> {
    let stored: Option<Summary> = transaction.get().primary(0_u8)?;
    let mut summary = stored.clone().unwrap_or_else(blank_summary);
    summary.repositories = transaction.len().primary::<Repository>()?;
    if stored.as_ref() != Some(&summary) {
        transaction.upsert(summary)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use hoard_core::{Base, Role};

    use super::*;

    fn named(name: &str, group_bytes: Option<u64>, named_by_repo: bool) -> ArtifactRecord {
        ArtifactRecord {
            id: 1,
            identity: 7,
            artifact: Artifact {
                id: ArtifactId::new(1),
                blob: BlobId::new(1),
                sha256: None,
                size: 1_024,
                format: ArtifactFormat::Safetensors,
                role: Role::Lora,
                sub_role: None,
                role_confidence: 0.99,
                base: Base::any(),
                base_label: None,
                base_confidence: 0.9,
                precision: None,
                quant: None,
                rank: None,
                alpha: None,
                params: None,
                context: None,
                trigger: None,
            },
            name: name.to_owned(),
            org: None,
            path_count: 1,
            copies: 1,
            group_bytes,
            incomplete: false,
            named_by_repo,
            shard_count: None,
            triage_cleared: false,
            rejected: false,
            first_seen: "2026-01-01T00:00:00Z".to_owned(),
        }
    }

    #[test]
    fn a_repository_naming_beats_a_path_naming_at_equal_bytes() {
        let by_repo = named("Ministral-8B-Instruct", None, true);
        let lexically_smaller = named("consolidated.safetensors", None, false);
        let lexically_larger = named("zzz/model.safetensors", None, false);

        assert!(
            names_better(&by_repo, &lexically_smaller),
            "the repository names the model even when a path sorts ahead of it"
        );
        assert!(
            !names_better(&lexically_smaller, &by_repo),
            "and the same pair settles the same way whichever the walk reaches first"
        );
        assert!(names_better(&by_repo, &lexically_larger));
        assert!(!names_better(&lexically_larger, &by_repo));
    }

    #[test]
    fn a_naming_is_ranked_by_what_it_was_not_by_the_repository_being_walked() {
        let by_repo = named("Ministral-8B-Instruct", None, true);
        let by_path = named("consolidated.safetensors", None, false);

        assert!(
            names_better(&by_repo, &by_path) && !names_better(&by_path, &by_repo),
            "each record carries its own answer, so no third repository can reorder the pair"
        );

        let coincidence = named("Ministral-8B-Instruct", None, false);
        assert!(
            names_better(&by_repo, &coincidence),
            "a name that merely reads like a repository's is still a path naming"
        );
    }

    #[test]
    fn more_content_still_outranks_a_repository_naming() {
        let by_repo = named("Ministral-8B-Instruct", None, true);
        let whole_set = named("model.safetensors", Some(8_192), false);

        assert!(
            names_better(&whole_set, &by_repo),
            "a set holding the whole model names it over a repository naming one file of it"
        );
    }
}
