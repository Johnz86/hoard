use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;

use rayon::prelude::{
    IndexedParallelIterator, IntoParallelIterator, IntoParallelRefIterator, ParallelIterator,
};

use hoard_core::{ArtifactFormat, GitSha1, Sha256, TensorSummary};

use crate::catalog::hashing::Stop;
use crate::catalog::naming::{
    InferenceContext, Naming, artifact_name, content_hashes, disambiguate_labels, index_document,
    inference_path, shard_label,
};
use crate::config::HashPolicy;
use crate::discover::DiscoveredEntry;
use crate::error::{IndexError, Result};
use crate::infer::{self, InferInput, Inference};
use crate::shard::{self, ShardCandidate, ShardSet, file_name};
use crate::sniff::{self, SniffResult};

use super::CHUNK;
use super::store::EntryContext;

/// What this disk holds for one discovered path once sniffing has run. A file can be on
/// disk yet unreadable, and an LFS-tracked file can be a fetched weight or a pointer stub.
pub(in crate::catalog) struct Presence {
    pub(in crate::catalog) abs_path: Option<PathBuf>,
    pub(in crate::catalog) readable: bool,
    pub(in crate::catalog) size: u64,
    pub(in crate::catalog) sha256: Option<Sha256>,
    pub(in crate::catalog) git_sha1: Option<GitSha1>,
    pub(in crate::catalog) lfs_pointer: bool,
    pub(in crate::catalog) pointer_bytes: bool,
}

impl Presence {
    pub(in crate::catalog) fn of(entry: &DiscoveredEntry, sniffed: Option<&SniffResult>) -> Self {
        let pointer = sniffed
            .and_then(|result| result.lfs_pointer.as_ref())
            .filter(|_| entry.sha256.is_none());
        match pointer {
            Some(pointer) => Self {
                abs_path: None,
                readable: false,
                size: pointer.size,
                sha256: Some(pointer.sha256),
                git_sha1: entry.git_sha1,
                lfs_pointer: true,
                pointer_bytes: true,
            },
            None => Self {
                abs_path: entry.abs_path.clone(),
                readable: sniffed.is_some(),
                size: entry.size,
                sha256: entry.sha256,
                git_sha1: entry.git_sha1,
                lfs_pointer: entry.lfs_pointer,
                pointer_bytes: false,
            },
        }
    }

    pub(super) fn present(&self) -> bool {
        self.readable
    }
}

/// A tensor container whose header parsed and reported nothing: a placeholder, not a model.
///
/// Only safetensors and GGUF answer this question: their sniffers yield no summary at all
/// for a header they could not read, so an empty summary means the header was read and held
/// nothing. A pickle container yields a summary either way, and a file hoard cannot parse
/// must still reach triage rather than quietly stop being an artifact.
fn has_no_tensors(format: ArtifactFormat, tensors: Option<&TensorSummary>) -> bool {
    matches!(format, ArtifactFormat::Safetensors | ArtifactFormat::Gguf)
        && tensors.is_some_and(|summary| summary.tensor_count == 0)
}

pub(in crate::catalog) fn creates_artifact(
    format: ArtifactFormat,
    presence: &Presence,
    role: &EntryRole,
    tensors: Option<&TensorSummary>,
) -> bool {
    format.is_artifact()
        && !presence.pointer_bytes
        && !matches!(role, EntryRole::Member)
        && !has_no_tensors(format, tensors)
}

/// What one file that becomes an artifact contributes: only such a file pays for inference
/// and a content identity.
pub(super) struct ArtifactDraft {
    pub(super) identity: u128,
    pub(super) inference: Inference,
}

pub(super) struct PreparedEntry<'a> {
    pub(super) discovered: &'a DiscoveredEntry,
    pub(super) sniffed: Option<SniffResult>,
    pub(super) presence: Presence,
    pub(super) artifact: Option<ArtifactDraft>,
    pub(super) role: EntryRole,
    pub(super) org: Option<String>,
    pub(super) name: String,
    pub(super) named_by_repo: bool,
}

/// One model split across files, as the revision's grouping pass settled it.
pub(in crate::catalog) struct GroupFacts {
    label: String,
    pub(super) bytes: u64,
    pub(super) shard_count: u32,
    pub(super) tensors: Option<TensorSummary>,
    pub(super) incomplete: bool,
}

/// Which part one file plays: a file of its own, the shard the artifact rides on, or one
/// more piece of a model another shard already stands for.
pub(in crate::catalog) enum EntryRole {
    Standalone,
    Anchor(GroupFacts),
    Member,
}

/// Everything the per entry pass of one revision reads besides the entry itself.
pub(super) struct RevisionPlan<'a> {
    pub(super) naming: Naming<'a>,
    pub(super) context: InferenceContext,
}

/// The documents a revision holds and the inference context they make. They are few and
/// small, so every later unit is described without the revision's weights being held.
pub(super) struct RevisionDocuments {
    context: InferenceContext,
    index_documents: HashMap<usize, serde_json::Value>,
}

/// Reads only the documents that describe a revision: cards, configs and index files.
pub(super) fn revision_documents(entries: &[DiscoveredEntry]) -> RevisionDocuments {
    let positions: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| is_document(entry))
        .map(|(position, _)| position)
        .collect();
    let sniffed: Vec<Option<SniffResult>> = positions
        .par_iter()
        .map(|position| sniff_entry(&entries[*position]))
        .collect();
    let context = InferenceContext::from_results(positions.iter().zip(&sniffed).filter_map(
        |(position, result)| {
            result
                .as_ref()
                .map(|result| (entries[*position].rel_path.as_str(), result))
        },
    ));
    let index_documents = positions
        .iter()
        .zip(&sniffed)
        .filter(|(position, _)| shard::is_index_document(file_name(&entries[**position].rel_path)))
        .filter_map(|(position, result)| {
            index_document(&entries[*position], result.as_ref())
                .map(|document| (*position, document))
        })
        .collect();
    RevisionDocuments {
        context,
        index_documents,
    }
}

fn is_document(entry: &DiscoveredEntry) -> bool {
    let name = file_name(&entry.rel_path);
    shard::is_index_document(name)
        || matches!(
            sniff::name_format(name),
            Some(
                ArtifactFormat::Readme
                    | ArtifactFormat::Config
                    | ArtifactFormat::Diffusers
                    | ArtifactFormat::Transformers
            )
        )
}

fn names_an_artifact(entry: &DiscoveredEntry) -> bool {
    sniff::name_format(file_name(&entry.rel_path)).is_some_and(ArtifactFormat::is_artifact)
}

/// Shard sets from names and index documents alone. Whether a set's anchor really holds
/// weights is decided when the set is sniffed as a unit.
pub(super) fn shard_sets_by_name(
    entries: &[DiscoveredEntry],
    documents: &RevisionDocuments,
) -> Vec<ShardSet> {
    let mut positions = Vec::new();
    let mut candidates = Vec::new();
    for (position, entry) in entries.iter().enumerate() {
        if entry.abs_path.is_none() || !shard::is_shard_file(file_name(&entry.rel_path)) {
            continue;
        }
        let mut candidate = ShardCandidate::new(entry.rel_path.as_str());
        if let Some(document) = documents.index_documents.get(&position) {
            candidate = candidate.with_index_document(document);
        }
        positions.push(position);
        candidates.push(candidate);
    }
    shard::group_shards(&candidates)
        .into_iter()
        .map(|set| ShardSet {
            members: set
                .members
                .iter()
                .map(|member| positions[*member])
                .collect(),
            ..set
        })
        .collect()
}

/// The largest amount of a revision that is sniffed, prepared and committed at once.
pub(super) enum Unit {
    Set(ShardSet),
    Chunk(Vec<usize>),
}

impl Unit {
    pub(super) fn positions(&self) -> &[usize] {
        match self {
            Unit::Set(set) => &set.members,
            Unit::Chunk(positions) => positions,
        }
    }
}

pub(super) fn revision_units(entries: &[DiscoveredEntry], sets: Vec<ShardSet>) -> Vec<Unit> {
    let grouped: HashSet<usize> = sets
        .iter()
        .flat_map(|set| set.members.iter().copied())
        .collect();
    let standalone: Vec<usize> = (0..entries.len())
        .filter(|position| !grouped.contains(position))
        .collect();
    sets.into_iter()
        .map(Unit::Set)
        .chain(
            standalone
                .chunks(CHUNK)
                .map(|chunk| Unit::Chunk(chunk.to_vec())),
        )
        .collect()
}

/// How many artifacts a revision will hold, decided from file names before any weight is
/// read, because whether the repository names its single artifact must be settled once for
/// every unit.
pub(super) fn revision_plan<'a>(
    context: &EntryContext<'a>,
    documents: RevisionDocuments,
    units: &[Unit],
) -> RevisionPlan<'a> {
    let entries = &context.revision.entries;
    let artifacts: usize = units
        .iter()
        .map(|unit| match unit {
            Unit::Set(_) => 1,
            Unit::Chunk(positions) => positions
                .iter()
                .filter(|position| names_an_artifact(&entries[**position]))
                .count(),
        })
        .sum();
    RevisionPlan {
        naming: Naming {
            repo: &context.repo.repo,
            root_path: &context.repo.root_path,
            kind: context.kind,
            single: artifacts <= 1,
        },
        context: documents.context,
    }
}

/// One label per unit, in unit order: a set's disambiguated label, and nothing for a chunk
/// of files that each stand for themselves.
pub(super) fn set_labels(
    naming: &Naming<'_>,
    units: &[Unit],
    entries: &[DiscoveredEntry],
) -> Vec<Option<String>> {
    let sets: Vec<&ShardSet> = units
        .iter()
        .filter_map(|unit| match unit {
            Unit::Set(set) => Some(set),
            Unit::Chunk(_) => None,
        })
        .collect();
    let mut labels: Vec<String> = sets.iter().map(|set| shard_label(naming, set)).collect();
    let member_counts: Vec<usize> = sets.iter().map(|set| set.members.len()).collect();
    let anchors: Vec<&str> = sets
        .iter()
        .map(|set| {
            entries
                .get(set.anchor())
                .map_or("", |entry| file_name(&entry.rel_path))
        })
        .collect();
    disambiguate_labels(&mut labels, &member_counts, &anchors);
    let mut per_unit: Vec<Option<String>> = Vec::with_capacity(units.len());
    per_unit.extend(labels.into_iter().map(Some));
    per_unit.resize(units.len(), None);
    per_unit
}

/// One artifact per set, described by the union of its shard headers and weighing them all.
fn group_facts(
    set: &ShardSet,
    sniffed: &[Option<SniffResult>],
    presences: &[Presence],
    label: String,
) -> GroupFacts {
    let parts: Vec<&TensorSummary> = sniffed
        .iter()
        .filter_map(|result| result.as_ref())
        .filter_map(|result| result.tensors.as_ref())
        .collect();
    GroupFacts {
        label,
        bytes: presences.iter().map(|presence| presence.size).sum(),
        shard_count: u32::try_from(presences.len()).unwrap_or(u32::MAX),
        tensors: (!parts.is_empty()).then(|| shard::merge_tensor_summaries(&parts)),
        incomplete: set.is_incomplete(),
    }
}

/// A set whose anchor turned out to hold no weights is not a model: its files go back to
/// standing for themselves.
fn unit_roles(
    unit: &Unit,
    sniffed: &[Option<SniffResult>],
    presences: &[Presence],
    label: Option<&str>,
) -> Vec<EntryRole> {
    let alone = || (0..sniffed.len()).map(|_| EntryRole::Standalone).collect();
    let set = match unit {
        Unit::Set(set) => set,
        Unit::Chunk(_) => return alone(),
    };
    let anchor = sniffed.first().and_then(Option::as_ref);
    let anchored = anchor.is_some_and(|result| result.format.is_artifact())
        && presences
            .first()
            .is_some_and(|presence| !presence.pointer_bytes)
        && !anchor.is_some_and(|result| has_no_tensors(result.format, result.tensors.as_ref()));
    if !anchored {
        return alone();
    }
    let facts = group_facts(
        set,
        sniffed,
        presences,
        label.unwrap_or_default().to_owned(),
    );
    std::iter::once(EntryRole::Anchor(facts))
        .chain(std::iter::repeat_with(|| EntryRole::Member).take(sniffed.len().saturating_sub(1)))
        .collect()
}

/// Sniffs one unit in parallel and prepares its files against the revision's documents, so
/// a README or config beside the weights describes them. Nothing larger than a unit is read
/// at once.
pub(super) fn prepare_unit<'a>(
    context: &EntryContext<'a>,
    plan: &RevisionPlan<'_>,
    unit: &Unit,
    label: Option<&str>,
    stop: &Stop,
) -> Result<Vec<PreparedEntry<'a>>> {
    let entries = &context.revision.entries;
    let positions = unit.positions();
    let sniffed: Vec<Option<SniffResult>> = positions
        .par_iter()
        .map(|position| sniff_entry(&entries[*position]))
        .collect();
    let mut presences: Vec<Presence> = positions
        .iter()
        .zip(&sniffed)
        .map(|(position, result)| Presence::of(&entries[*position], result.as_ref()))
        .collect();
    match context.settings.policy {
        HashPolicy::Now => hash_presences(&mut presences, context.settings.threshold, stop),
        HashPolicy::Background | HashPolicy::Never => {}
    }
    let roles = unit_roles(unit, &sniffed, &presences, label);
    positions
        .par_iter()
        .zip(sniffed.into_par_iter())
        .zip(presences.into_par_iter())
        .zip(roles.into_par_iter())
        .map(|(((position, sniffed), presence), role)| {
            prepare_entry(&entries[*position], sniffed, presence, role, plan)
        })
        .collect()
}

/// Reads the content hashes of every file of one unit that does not already carry them, so
/// the identity the unit's artifacts take is a content identity from this scan onwards.
fn hash_presences(presences: &mut [Presence], threshold: u64, stop: &Stop) {
    let bytes_this_phase_does_not_report = AtomicU64::new(0);
    let digests: Vec<Option<(Sha256, Option<GitSha1>)>> = presences
        .par_iter()
        .map(|presence| {
            let path = presence
                .abs_path
                .as_deref()
                .filter(|_| presence.sha256.is_none())?;
            match content_hashes(
                path,
                presence.size,
                threshold,
                stop,
                &bytes_this_phase_does_not_report,
            ) {
                Ok(hashes) => hashes,
                Err(error) => {
                    tracing::debug!(path = %path.display(), %error, "file is unhashable");
                    None
                }
            }
        })
        .collect();
    for (presence, digest) in presences.iter_mut().zip(digests) {
        let Some((sha256, git_sha1)) = digest else {
            continue;
        };
        presence.sha256 = Some(sha256);
        presence.git_sha1 = presence.git_sha1.or(git_sha1);
    }
}

fn sniff_entry(entry: &DiscoveredEntry) -> Option<SniffResult> {
    let path = entry.abs_path.as_deref()?;
    match sniff::sniff_named(path, file_name(&entry.rel_path)) {
        Ok(result) => Some(result),
        Err(error) => {
            tracing::debug!(path = %path.display(), %error, "candidate is unreadable");
            None
        }
    }
}

fn prepare_entry<'a>(
    entry: &'a DiscoveredEntry,
    sniffed: Option<SniffResult>,
    presence: Presence,
    role: EntryRole,
    plan: &RevisionPlan<'_>,
) -> Result<PreparedEntry<'a>> {
    let format = sniffed
        .as_ref()
        .map_or(ArtifactFormat::Unknown, |result| result.format);
    let (size, tensors, label) = match &role {
        EntryRole::Anchor(facts) => (
            facts.bytes,
            facts.tensors.as_ref(),
            Some(facts.label.as_str()),
        ),
        EntryRole::Standalone | EntryRole::Member => (
            presence.size,
            sniffed.as_ref().and_then(|result| result.tensors.as_ref()),
            None,
        ),
    };
    let creates_artifact = creates_artifact(format, &presence, &role, tensors);
    let path = inference_path(
        plan.naming.root_path,
        &entry.rel_path,
        entry.abs_path.as_deref(),
    );
    let file = file_name(&entry.rel_path);
    let artifact = creates_artifact
        .then(|| -> Result<ArtifactDraft> {
            let mut input = InferInput::new(format, &path, file, size);
            input.tensors = tensors;
            if let Some(sniffed) = &sniffed {
                input.config = sniffed
                    .config
                    .as_ref()
                    .or_else(|| plan.context.config_for(&entry.rel_path));
            }
            input.quant_config = plan.context.quant_config_for(&entry.rel_path);
            input.frontmatter = plan.context.frontmatter();
            Ok(ArtifactDraft {
                identity: identity(entry, &presence)?,
                inference: infer::infer(&input),
            })
        })
        .transpose()?;
    let (org, name) = artifact_name(&plan.naming, &entry.rel_path, label);
    let named_by_repo = name == plan.naming.repo.name();
    Ok(PreparedEntry {
        discovered: entry,
        artifact,
        name,
        named_by_repo,
        org,
        presence,
        sniffed,
        role,
    })
}

/// What the catalog knows one file's content by: its sha256, else its git blob sha, else
/// the canonical path it sits at, hashed from the path's own bytes. A path outside UTF-8
/// therefore hashes differently from the lossy text that renders it, once the walker admits
/// one.
pub(in crate::catalog) fn identity(entry: &DiscoveredEntry, presence: &Presence) -> Result<u128> {
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    match (presence.sha256, entry.git_sha1, entry.abs_path.as_deref()) {
        (Some(sha), _, _) => hasher.update(format!("sha256:{sha}").as_bytes()),
        (None, Some(sha), _) => hasher.update(format!("git:{sha}").as_bytes()),
        (None, None, Some(path)) => {
            hasher.update(b"path:");
            hasher.update(
                path.canonicalize()
                    .unwrap_or_else(|_| path.to_path_buf())
                    .as_os_str()
                    .as_encoded_bytes(),
            );
        }
        (None, None, None) => {
            return Err(IndexError::Catalog(format!(
                "absent entry has no content identity: {}",
                entry.rel_path
            )));
        }
    }
    Ok(hasher.digest128())
}
