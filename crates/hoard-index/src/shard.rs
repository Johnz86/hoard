//! Shard sets: the files one model was split across, seen as one thing.
//!
//! A large model is written as `<stem>-<n>-of-<total>.<ext>`, beside a `<stem>.index.json`
//! whose `weight_map` names every piece. Only the first piece carries the structure
//! inference reads, so a file per artifact leaves every other piece unclassified. A set is
//! detected inside one revision and one directory, becomes a single artifact anchored on
//! its lowest numbered shard, and describes itself from the union of the shard headers.
//! Blobs stay per file, because deduplication and serving are per file.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use hoard_core::{TensorSample, TensorSummary};
use serde_json::Value;

use crate::sniff::{MAX_PREFIXES, MAX_SAMPLES};

/// The containers a model is split across. A format absent here is never a shard, whatever
/// its name spells.
const SHARD_EXTENSIONS: [&str; 5] = ["safetensors", "bin", "pth", "gguf", "parquet"];

const INDEX_SUFFIX: &str = ".index.json";

/// What separates a shard's own number from the size of the set it belongs to.
const SHARD_SEPARATOR: &str = "-of-";

/// A lone file is not a set, whatever its name claims.
const MIN_MEMBERS: usize = 2;

/// One file's place in a shard set, as its name spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShardName<'a> {
    pub stem: &'a str,
    pub extension: &'a str,
    pub index: u32,
    pub total: u32,
}

/// A file grouping may consider: where it sits in the revision, and the parsed document
/// when it is a `*.index.json` naming the pieces of a model.
#[derive(Debug, Clone, Copy)]
pub struct ShardCandidate<'a> {
    pub rel_path: &'a str,
    pub index_document: Option<&'a Value>,
}

impl<'a> ShardCandidate<'a> {
    pub fn new(rel_path: &'a str) -> Self {
        Self {
            rel_path,
            index_document: None,
        }
    }

    pub fn with_index_document(mut self, document: &'a Value) -> Self {
        self.index_document = Some(document);
        self
    }
}

/// The files of one model, as positions into the candidates they were found among.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardSet {
    /// Lowest numbered shard first: that one anchors the artifact.
    pub members: Vec<usize>,
    /// The directory the shards share, relative to the revision root.
    pub directory: String,
    /// What the set would be called without its shard suffix.
    pub stem: String,
    /// The file extension its members share, without the dot.
    pub extension: String,
    /// How many files the set says it has: the `-of-<total>` its names spell, or the count
    /// of distinct files its `weight_map` points at. `None` when nothing declared one.
    pub declared_total: Option<u32>,
}

impl ShardSet {
    /// The member whose blob the artifact is identified by.
    pub fn anchor(&self) -> usize {
        self.members.first().copied().unwrap_or_default()
    }

    /// True when fewer files were found than the set declares, which is what a partial
    /// download looks like: the row must not read as a model that can be served.
    pub fn is_incomplete(&self) -> bool {
        self.declared_total
            .is_some_and(|total| u32::try_from(self.members.len()).unwrap_or(u32::MAX) < total)
    }
}

/// Reads `<stem>-<n>-of-<total>.<ext>`, and nothing else. Both numbers must be digits, so
/// `something-of-note.safetensors` is a file with an unlucky name rather than a shard.
pub fn parse_shard_name(file_name: &str) -> Option<ShardName<'_>> {
    let (base, extension) = file_name.rsplit_once('.')?;
    if !SHARD_EXTENSIONS
        .iter()
        .any(|known| extension.eq_ignore_ascii_case(known))
    {
        return None;
    }
    let (head, total) = base.rsplit_once(SHARD_SEPARATOR)?;
    let (stem, index) = head.rsplit_once('-')?;
    if stem.is_empty() {
        return None;
    }
    Some(ShardName {
        stem,
        extension,
        index: digits(index)?,
        total: digits(total)?,
    })
}

/// True for a container a model is ever split across, which is the only kind of file
/// grouping considers.
pub fn is_shardable(file_name: &str) -> bool {
    file_name.rsplit_once('.').is_some_and(|(_, extension)| {
        SHARD_EXTENSIONS
            .iter()
            .any(|known| extension.eq_ignore_ascii_case(known))
    })
}

/// True for the document that names the pieces of a model rather than holding any.
pub fn is_index_document(file_name: &str) -> bool {
    file_name.len() > INDEX_SUFFIX.len() && file_name.ends_with(INDEX_SUFFIX)
}

/// True for any file grouping considers: a container a model splits across, or the index
/// document that names the pieces.
pub fn is_shard_file(file_name: &str) -> bool {
    is_shardable(file_name) || is_index_document(file_name)
}

/// Groups the candidates into the sets they belong to, a declared `weight_map` first and
/// the naming convention for whatever it leaves over. Sets are ordered by their anchor.
pub fn group_shards(candidates: &[ShardCandidate<'_>]) -> Vec<ShardSet> {
    let positions: HashMap<&str, usize> = candidates
        .iter()
        .enumerate()
        .map(|(position, candidate)| (candidate.rel_path, position))
        .collect();
    let mut claimed = vec![false; candidates.len()];
    let mut sets: Vec<ShardSet> = candidates
        .iter()
        .filter_map(|candidate| declared_set(candidate, candidates, &positions, &mut claimed))
        .collect();
    sets.extend(named_sets(candidates, &mut claimed));
    sets.sort_unstable_by_key(ShardSet::anchor);
    sets
}

/// The set a `*.index.json` declares, which outranks what the file names suggest.
fn declared_set(
    candidate: &ShardCandidate<'_>,
    candidates: &[ShardCandidate<'_>],
    positions: &HashMap<&str, usize>,
    claimed: &mut [bool],
) -> Option<ShardSet> {
    let document = candidate.index_document?;
    let file = file_name(candidate.rel_path);
    if !is_index_document(file) {
        return None;
    }
    let directory = directory_of(candidate.rel_path);
    let named = weight_map_files(document);
    let declared_total = u32::try_from(named.len()).ok();
    let mut members: Vec<usize> = named
        .into_iter()
        .filter_map(|name| positions.get(join_path(directory, name).as_str()).copied())
        .filter(|position| !claimed[*position])
        .collect();
    members.sort_unstable();
    members.dedup();
    if members.len() < MIN_MEMBERS {
        return None;
    }
    order_members(&mut members, candidates);
    for position in &members {
        claimed[*position] = true;
    }
    let extension = members
        .first()
        .and_then(|anchor| file_name(candidates[*anchor].rel_path).rsplit_once('.'))
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    Some(ShardSet {
        members,
        directory: directory.to_owned(),
        stem: index_stem(file).to_owned(),
        extension,
        declared_total,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SetKey<'a> {
    directory: &'a str,
    stem: &'a str,
    extension: String,
    total: u32,
}

/// The sets the naming convention alone proves: one directory, one stem, one total.
fn named_sets<'a>(candidates: &[ShardCandidate<'a>], claimed: &mut [bool]) -> Vec<ShardSet> {
    let mut buckets: BTreeMap<SetKey<'a>, Vec<(u32, usize)>> = BTreeMap::new();
    for (position, candidate) in candidates.iter().enumerate() {
        if claimed[position] {
            continue;
        }
        let Some(name) = parse_shard_name(file_name(candidate.rel_path)) else {
            continue;
        };
        if usize::try_from(name.total).unwrap_or(usize::MAX) < MIN_MEMBERS {
            continue;
        }
        let key = SetKey {
            directory: directory_of(candidate.rel_path),
            stem: name.stem,
            extension: name.extension.to_ascii_lowercase(),
            total: name.total,
        };
        buckets.entry(key).or_default().push((name.index, position));
    }
    buckets
        .into_iter()
        .filter_map(|(key, mut ordered)| {
            if ordered.len() < MIN_MEMBERS {
                return None;
            }
            ordered.sort_unstable();
            let members: Vec<usize> = ordered.into_iter().map(|(_, position)| position).collect();
            for position in &members {
                claimed[*position] = true;
            }
            Some(ShardSet {
                members,
                directory: key.directory.to_owned(),
                stem: key.stem.to_owned(),
                extension: key.extension,
                declared_total: Some(key.total),
            })
        })
        .collect()
}

/// Puts the lowest numbered shard first, falling back to the path for a member whose name
/// carries no number at all.
fn order_members(members: &mut [usize], candidates: &[ShardCandidate<'_>]) {
    members.sort_by(|left, right| {
        let (left, right) = (candidates[*left].rel_path, candidates[*right].rel_path);
        shard_index(left)
            .cmp(&shard_index(right))
            .then_with(|| left.cmp(right))
    });
}

fn shard_index(rel_path: &str) -> u32 {
    parse_shard_name(file_name(rel_path)).map_or(u32::MAX, |name| name.index)
}

/// The distinct files a `weight_map` points its tensors at.
fn weight_map_files(document: &Value) -> BTreeSet<&str> {
    document
        .get("weight_map")
        .and_then(Value::as_object)
        .map(|map| map.values().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// `model.safetensors.index.json` and `model.index.json` both describe `model`.
fn index_stem(file_name: &str) -> &str {
    let base = file_name
        .strip_suffix(INDEX_SUFFIX)
        .unwrap_or(file_name)
        .trim_end_matches('.');
    match base.rsplit_once('.') {
        Some((stem, extension))
            if SHARD_EXTENSIONS
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known)) =>
        {
            stem
        }
        Some(_) | None => base,
    }
}

fn digits(value: &str) -> Option<u32> {
    let numeric = !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit());
    numeric.then(|| value.parse().ok()).flatten()
}

/// The tensor summary of a whole model, formed from the header of each of its shards.
///
/// Counts and parameters add up, samples are drawn a round at a time so no shard fills the
/// budget alone, and the first shard to carry a metadata key keeps it.
pub fn merge_tensor_summaries(parts: &[&TensorSummary]) -> TensorSummary {
    let mut merged = TensorSummary::default();
    let mut prefixes: BTreeMap<&str, u32> = BTreeMap::new();
    for part in parts {
        merged.tensor_count = merged.tensor_count.saturating_add(part.tensor_count);
        merged.params = merged.params.saturating_add(part.params);
        for (dtype, count) in &part.dtypes {
            let total = merged.dtypes.entry(dtype.clone()).or_default();
            *total = total.saturating_add(*count);
        }
        for (prefix, count) in &part.key_prefixes {
            let total = prefixes.entry(prefix.as_str()).or_default();
            *total = total.saturating_add(*count);
        }
        for (key, value) in &part.metadata {
            merged
                .metadata
                .entry(key.clone())
                .or_insert_with(|| value.clone());
        }
    }
    merged.key_prefixes = ranked_prefixes(prefixes);
    merged.samples = spread_samples(parts);
    merged
}

fn ranked_prefixes(counted: BTreeMap<&str, u32>) -> Vec<(String, u32)> {
    let mut ranked: Vec<(String, u32)> = counted
        .into_iter()
        .map(|(prefix, count)| (prefix.to_owned(), count))
        .collect();
    ranked.sort_unstable_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    ranked.truncate(MAX_PREFIXES);
    ranked
}

fn spread_samples(parts: &[&TensorSummary]) -> Vec<TensorSample> {
    let depth = parts
        .iter()
        .map(|part| part.samples.len())
        .max()
        .unwrap_or_default();
    let mut samples: Vec<TensorSample> = Vec::with_capacity(MAX_SAMPLES.min(depth * parts.len()));
    for round in 0..depth {
        for part in parts {
            if samples.len() >= MAX_SAMPLES {
                break;
            }
            if let Some(sample) = part.samples.get(round) {
                samples.push(sample.clone());
            }
        }
    }
    samples.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    samples.dedup_by(|left, right| left.name == right.name);
    samples
}

pub(crate) fn file_name(rel_path: &str) -> &str {
    rel_path.rsplit('/').next().unwrap_or(rel_path)
}

/// Everything before the last segment, empty at the revision root.
pub(crate) fn directory_of(rel_path: &str) -> &str {
    rel_path
        .rsplit_once('/')
        .map_or("", |(directory, _)| directory)
}

fn join_path(directory: &str, name: &str) -> String {
    match directory.is_empty() {
        true => name.to_owned(),
        false => format!("{directory}/{name}"),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn candidates<'a>(paths: &'a [&'a str]) -> Vec<ShardCandidate<'a>> {
        paths.iter().map(|path| ShardCandidate::new(path)).collect()
    }

    fn summary(names: &[&str], dtype: &str, params: u64) -> TensorSummary {
        TensorSummary {
            tensor_count: names.len() as u32,
            params,
            dtypes: [(dtype.to_owned(), names.len() as u64 * 4)]
                .into_iter()
                .collect(),
            key_prefixes: names
                .iter()
                .map(|name| (crate::sniff::key_prefix(name).to_owned(), 1))
                .collect(),
            samples: names
                .iter()
                .map(|name| TensorSample {
                    name: (*name).to_owned(),
                    dtype: dtype.to_owned(),
                    shape: vec![2, 2],
                })
                .collect(),
            metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn a_shard_name_needs_digits_on_both_sides() {
        let name = parse_shard_name("model-00001-of-00040.safetensors").expect("a shard name");
        assert_eq!(name.stem, "model");
        assert_eq!(name.extension, "safetensors");
        assert_eq!(name.index, 1);
        assert_eq!(name.total, 40);

        assert_eq!(
            parse_shard_name("train-00000-of-00035.parquet").map(|name| name.total),
            Some(35)
        );
        assert!(parse_shard_name("something-of-note.safetensors").is_none());
        assert!(parse_shard_name("model.safetensors").is_none());
        assert!(parse_shard_name("-00001-of-00002.gguf").is_none());
        assert!(parse_shard_name("notes-1-of-2.txt").is_none());
    }

    #[test]
    fn the_naming_convention_groups_one_directory_at_a_time() {
        let paths = [
            "model-00001-of-00003.safetensors",
            "model-00002-of-00003.safetensors",
            "model-00003-of-00003.safetensors",
            "encoder/model-00001-of-00002.safetensors",
            "encoder/model-00002-of-00002.safetensors",
            "config.json",
        ];
        let sets = group_shards(&candidates(&paths));
        assert_eq!(sets.len(), 2);
        assert_eq!(sets[0].members, vec![0, 1, 2]);
        assert_eq!(sets[0].directory, "");
        assert_eq!(sets[0].stem, "model");
        assert_eq!(sets[1].members, vec![3, 4]);
        assert_eq!(sets[1].directory, "encoder");
    }

    #[test]
    fn a_set_missing_members_reports_the_total_it_declared() {
        let paths = [
            "model-00001-of-00005.safetensors",
            "model-00002-of-00005.safetensors",
        ];
        let sets = group_shards(&candidates(&paths));
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].declared_total, Some(5));
        assert!(
            sets[0].is_incomplete(),
            "two of five on disk is a half-finished download, not a two shard model"
        );

        let whole = [
            "model-00001-of-00002.safetensors",
            "model-00002-of-00002.safetensors",
        ];
        assert!(!group_shards(&candidates(&whole))[0].is_incomplete());
    }

    #[test]
    fn a_weight_map_counts_the_files_it_names_even_when_one_is_missing() {
        let document = json!({
            "weight_map": {
                "a.weight": "piece-one.safetensors",
                "b.weight": "piece-two.safetensors",
                "c.weight": "piece-three.safetensors",
            }
        });
        let paths = [
            "model.safetensors.index.json",
            "piece-one.safetensors",
            "piece-two.safetensors",
        ];
        let mut found = candidates(&paths);
        found[0] = found[0].with_index_document(&document);

        let sets = group_shards(&found);
        assert_eq!(sets[0].declared_total, Some(3));
        assert!(sets[0].is_incomplete());
    }

    #[test]
    fn a_lone_shard_and_a_single_file_set_stay_alone() {
        let paths = [
            "model-00007-of-00014.safetensors",
            "data/train-00000-of-00001.parquet",
            "model.safetensors",
            "something-of-note.safetensors",
        ];
        assert!(group_shards(&candidates(&paths)).is_empty());
    }

    #[test]
    fn a_weight_map_names_the_set_whatever_the_files_are_called() {
        let document = json!({
            "weight_map": {
                "a.weight": "piece-one.safetensors",
                "b.weight": "piece-two.safetensors",
                "c.weight": "piece-one.safetensors",
                "d.weight": "missing.safetensors",
            }
        });
        let paths = [
            "model.safetensors.index.json",
            "piece-one.safetensors",
            "piece-two.safetensors",
        ];
        let mut found = candidates(&paths);
        found[0] = found[0].with_index_document(&document);

        let sets = group_shards(&found);
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].members, vec![1, 2]);
        assert_eq!(sets[0].stem, "model");
    }

    #[test]
    fn a_weight_map_claims_its_members_before_the_naming_convention_does() {
        let document = json!({
            "weight_map": {
                "a.weight": "model-00001-of-00002.safetensors",
                "b.weight": "model-00002-of-00002.safetensors",
            }
        });
        let paths = [
            "model-00002-of-00002.safetensors",
            "model.safetensors.index.json",
            "model-00001-of-00002.safetensors",
        ];
        let mut found = candidates(&paths);
        found[1] = found[1].with_index_document(&document);

        let sets = group_shards(&found);
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].members, vec![2, 0]);
        assert_eq!(sets[0].anchor(), 2);
    }

    #[test]
    fn an_incomplete_set_groups_what_is_there() {
        let paths = [
            "model-00001-of-00005.safetensors",
            "model-00002-of-00005.safetensors",
            "model-00004-of-00005.safetensors",
        ];
        let sets = group_shards(&candidates(&paths));
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].members, vec![0, 1, 2]);
    }

    #[test]
    fn only_a_container_a_model_splits_across_is_shardable() {
        assert!(is_shardable("model-00001-of-00002.safetensors"));
        assert!(is_shardable("train-00000-of-00035.PARQUET"));
        assert!(is_shardable("weights.pth"));
        assert!(!is_shardable("model.safetensors.index.json"));
        assert!(!is_shardable("README.md"));
        assert!(!is_shardable("weights"));
    }

    #[test]
    fn an_index_document_names_the_model_it_describes() {
        assert_eq!(index_stem("model.safetensors.index.json"), "model");
        assert_eq!(index_stem("pytorch_model.bin.index.json"), "pytorch_model");
        assert_eq!(index_stem("model.index.json"), "model");
        assert!(is_index_document("model.safetensors.index.json"));
        assert!(!is_index_document("model_index.json"));
        assert!(!is_index_document(".index.json"));
    }

    #[test]
    fn a_merged_summary_describes_every_shard_at_once() {
        let first = summary(&["model.layers.0.weight"], "F16", 1_000);
        let second = summary(&["model.layers.1.weight", "lm_head.weight"], "F16", 2_500);
        let merged = merge_tensor_summaries(&[&first, &second]);

        assert_eq!(merged.tensor_count, 3);
        assert_eq!(merged.params, 3_500);
        assert_eq!(merged.dtypes.get("F16"), Some(&12));
        assert_eq!(merged.samples.len(), 3);
        assert_eq!(
            merged
                .key_prefixes
                .iter()
                .find(|(prefix, _)| prefix == "model")
                .map(|(_, count)| *count),
            Some(2)
        );
    }

    #[test]
    fn a_merged_summary_keeps_a_sample_from_every_shard() {
        let parts: Vec<TensorSummary> = (0..8)
            .map(|shard| {
                let names: Vec<String> = (0..40)
                    .map(|index| format!("shard{shard}.block{index:02}.weight"))
                    .collect();
                let borrowed: Vec<&str> = names.iter().map(String::as_str).collect();
                summary(&borrowed, "BF16", 10)
            })
            .collect();
        let merged = merge_tensor_summaries(&parts.iter().collect::<Vec<&TensorSummary>>());

        assert_eq!(merged.samples.len(), MAX_SAMPLES);
        for shard in 0..8 {
            assert!(
                merged
                    .samples
                    .iter()
                    .any(|sample| sample.name.starts_with(&format!("shard{shard}."))),
                "every shard is represented in the samples"
            );
        }
    }

    #[test]
    fn a_shard_file_is_a_container_or_the_index_that_names_them() {
        assert!(is_shard_file("model-00001-of-00003.safetensors"));
        assert!(is_shard_file("model.safetensors.index.json"));
        assert!(is_shard_file("part-a.safetensors"));
        assert!(!is_shard_file("README.md"));
        assert!(!is_shard_file("config.json"));
    }
}
