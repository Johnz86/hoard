use native_db::transaction::{RTransaction, RwTransaction};

use hoard_core::{ArtifactId, Base, Evidence, EvidenceSource, Role, Sha256, State};

use crate::error::Result;
use crate::infer::lookup::{Refinement, listed_base, refine};
use crate::infer::trigger::MAX_TRIGGER_TAGS;
use crate::time::{now, parse_timestamp};

use super::hashing::content_key;
use super::models::{
    ArtifactDetail, ArtifactRecord, ArtifactRecordKey, Entry, EntryKey, LOCAL_NODE, Override,
};
use super::scan::{Counters, log_changes};
use super::{Catalog, Listed, LookupAnswer};

/// How long an answer of "asked, and unknown" stands before the same bytes are asked about
/// again. An uploader can list a file the day after a pass asked about it, so an unknown is
/// not forever; a found answer never expires, because these bytes cannot become other bytes.
const RETRY_UNKNOWN_DAYS: i64 = 30;

const SECONDS_PER_DAY: i64 = 24 * 60 * 60;

/// The field a lookup refines, and the one its evidence is filed under.
const LOOKUP_FIELD: &str = "base";

/// The other field a listing can settle, filed under the same source.
const TRIGGER_FIELD: &str = "trigger";

/// One artifact a lookup pass may ask about, with the hash that is all it ever sends.
#[derive(Debug, Clone, PartialEq)]
pub struct LookupCandidate {
    pub artifact: ArtifactId,
    pub sha256: Sha256,
}

impl Catalog {
    /// Up to `limit` hashed artifacts of a role a listing service carries, which no lookup
    /// has answered yet.
    ///
    /// An artifact the memo already answers is a candidate: applying what is remembered costs
    /// no request. One whose answer was "unknown" less than thirty days ago is not, nor is one
    /// that already carries a lookup's evidence, nor one whose base a user has decided: a
    /// verdict is never overwritten, so such an artifact would take a place in every window
    /// for ever and starve the artifacts behind it.
    pub fn lookup_candidates(&self, limit: usize) -> Result<Vec<LookupCandidate>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let memo = self.lookup_memo()?;
        let remembered = memo.reading()?;
        let asked_before = now().as_second() - RETRY_UNKNOWN_DAYS * SECONDS_PER_DAY;
        let read = self.database.r_transaction()?;
        let mut candidates = Vec::with_capacity(limit.min(64));
        for record in read.scan().primary::<ArtifactRecord>()?.all()? {
            if candidates.len() == limit {
                break;
            }
            let record: ArtifactRecord = record?;
            let Some(sha256) = record.artifact.sha256 else {
                continue;
            };
            if !is_listed_role(record.artifact.role) {
                continue;
            }
            if let Some(answer) = remembered.answer(&sha256)?
                && !worth_asking_again(&sha256, &answer, asked_before)
            {
                continue;
            }
            let verdict: Option<Override> = read.get().primary(record.id)?;
            if decides_the_base(verdict.as_ref())
                || carries_a_lookup(&read, record.id)?
                || !held_on_this_node(&read, record.id)?
            {
                continue;
            }
            candidates.push(LookupCandidate {
                artifact: ArtifactId::new(record.id),
                sha256,
            });
        }
        Ok(candidates)
    }

    /// Applies a batch of answers, and returns how many stored bases changed.
    ///
    /// One transaction for the batch, shaped like [`Catalog::put_override`]: the record is
    /// measured before it is written, the artifacts whose base moved are logged for the
    /// search index, and the index is refreshed once, only when something moved. Every answer
    /// leaves one piece of evidence behind whatever it decided, which is also what stops the
    /// artifact being a candidate again.
    pub fn apply_lookups(&self, answers: &[(Sha256, LookupAnswer)]) -> Result<usize> {
        if answers.is_empty() {
            return Ok(0);
        }
        let mut counters = Counters::load(&self.database, self.stored_triage()?)?;
        let transaction = self.database.rw_transaction()?;
        let mut changed = 0;
        for (sha256, answer) in answers {
            let Some(listed) = answer.found.as_ref() else {
                continue;
            };
            let Some(record) = artifact_of(&transaction, sha256)? else {
                continue;
            };
            let verdict: Option<Override> = transaction.get().primary(record.id)?;
            if decides_the_base(verdict.as_ref()) {
                continue;
            }
            let refinement = refine(&record.artifact.base, listed);
            let id = record.id;
            let held = record.artifact.base.clone();
            let trigger = match is_listed_role(record.artifact.role) {
                true => listed_trigger(&transaction, id, listed)?,
                false => None,
            };
            let mut record = record;
            let mut moved = false;
            match &refinement {
                Refinement::Refined { base, label } => {
                    record.artifact.base_label = spelled(base, label);
                    record.artifact.base = base.clone();
                    moved = true;
                }
                Refinement::Named { base, label } => {
                    record.artifact.base_label = spelled(base, label);
                    record.artifact.base = base.clone();
                    record.artifact.base_confidence = EvidenceSource::Lookup.weight();
                    moved = true;
                }
                Refinement::Unchanged | Refinement::Contradicted { .. } => {}
            }
            if let Some(words) = &trigger {
                record.artifact.trigger = Some(words.clone());
                moved = true;
            }
            if moved {
                counters.touch(&transaction, id)?;
                transaction.upsert(record)?;
                counters.pending.push(id);
                changed += 1;
            }
            record_listing(
                &transaction,
                ListingFacts {
                    id,
                    listed,
                    refinement: &refinement,
                    held: &held,
                    trigger: trigger.as_deref(),
                },
            )?;
        }
        log_changes(&transaction, &mut counters)?;
        counters.settle_summary(&transaction)?;
        transaction.commit()?;
        if changed > 0 {
            self.refresh_search(|_| {})?;
        }
        Ok(changed)
    }
}

/// The roles a listing service carries, and so the only roles a hash is ever sent for. A
/// language model, a dataset and a projector are never asked about.
const fn is_listed_role(role: Role) -> bool {
    match role {
        Role::Ckpt | Role::Lora | Role::Ctrl | Role::Vae | Role::Emb | Role::Upsc => true,
        Role::Llm | Role::Ds | Role::Proj | Role::Unk => false,
    }
}

/// Whether an answer already remembered leaves anything to do. A found listing is applied
/// from the memo without a request; an unknown stands for `RETRY_UNKNOWN_DAYS` days.
///
/// An instant that does not parse leaves the hash alone rather than asking about it on every
/// pass for ever, and says so, because a memo row nothing can read is a fault to see.
fn worth_asking_again(sha256: &Sha256, answer: &LookupAnswer, asked_before: i64) -> bool {
    if answer.found.is_some() {
        return true;
    }
    match parse_timestamp(&answer.asked_at) {
        Ok(asked_at) => asked_at.as_second() <= asked_before,
        Err(error) => {
            tracing::debug!(
                hash = %sha256,
                asked_at = %answer.asked_at,
                %error,
                "a lookup memo row carries an instant this build cannot read; it is left alone"
            );
            false
        }
    }
}

/// Whether a user's verdict settles this artifact's base, which no lookup ever overwrites.
fn decides_the_base(verdict: Option<&Override>) -> bool {
    verdict.is_some_and(|verdict| verdict.base.is_some())
}

fn carries_a_lookup(read: &RTransaction<'_>, id: i64) -> Result<bool> {
    let stored: Option<ArtifactDetail> = read.get().primary(id)?;
    Ok(stored.is_some_and(|detail| detail.evidence.iter().any(is_a_lookup_on_the_base)))
}

fn held_on_this_node(read: &RTransaction<'_>, id: i64) -> Result<bool> {
    let entries: Vec<Entry> = read
        .scan()
        .secondary::<Entry>(EntryKey::artifact_id)?
        .start_with(Some(id))?
        .collect::<native_db::db_type::Result<_>>()?;
    Ok(entries.iter().any(|entry| {
        entry.artifact_id == Some(id) && entry.node == LOCAL_NODE && entry.state != State::Remote
    }))
}

fn is_a_lookup_on_the_base(evidence: &Evidence) -> bool {
    evidence.source == EvidenceSource::Lookup && evidence.field == LOOKUP_FIELD
}

fn is_a_lookup_on_the_trigger(evidence: &Evidence) -> bool {
    evidence.source == EvidenceSource::Lookup && evidence.field == TRIGGER_FIELD
}

/// The record these bytes belong to. A hashed artifact is keyed by its content, so one answer
/// reaches one artifact however many paths hold it.
fn artifact_of(transaction: &RwTransaction<'_>, sha256: &Sha256) -> Result<Option<ArtifactRecord>> {
    let stored: Option<ArtifactRecord> = transaction
        .get()
        .secondary(ArtifactRecordKey::identity, content_key(sha256))?;
    Ok(stored.filter(|record| record.artifact.sha256.as_ref() == Some(sha256)))
}

/// The label a listing leaves on a record: the spelling the service writes, and nothing at
/// all when that is the key itself, which prints the way every rule-inferred base does.
fn spelled(base: &Base, label: &str) -> Option<String> {
    (label != base.as_str()).then(|| label.to_owned())
}

/// The trigger words a listing settles, or nothing when it lists none or when the artifact
/// already carries a stronger answer than a listing: `ss_tag_frequency` is the uploader's
/// own training metadata and outranks the page the file was published on, while a file-name
/// or front-matter guess does not. What an earlier listing wrote is not held against a
/// later one, so the words follow the listing the detail keeps.
fn listed_trigger(
    transaction: &RwTransaction<'_>,
    id: i64,
    listed: &Listed,
) -> Result<Option<String>> {
    if listed.trained_words.is_empty() {
        return Ok(None);
    }
    let stored: Option<ArtifactDetail> = transaction.get().primary(id)?;
    let held = stored
        .iter()
        .flat_map(|detail| detail.evidence.iter())
        .filter(|evidence| !is_a_lookup_on_the_trigger(evidence))
        .fold(0.0_f32, |strongest, evidence| {
            match evidence.field == TRIGGER_FIELD {
                true => strongest.max(evidence.confidence),
                false => strongest,
            }
        });
    if held >= EvidenceSource::Lookup.weight() {
        return Ok(None);
    }
    let words: Vec<&str> = listed
        .trained_words
        .iter()
        .map(|word| word.trim())
        .filter(|word| !word.is_empty())
        .take(MAX_TRIGGER_TAGS)
        .collect();
    Ok((!words.is_empty()).then(|| words.join(", ")))
}

/// What one answer leaves on the artifact's detail row: the listing itself, what it did to
/// the base, and the trigger words it settled when it settled any.
struct ListingFacts<'a> {
    id: i64,
    listed: &'a Listed,
    refinement: &'a Refinement,
    held: &'a Base,
    trigger: Option<&'a str>,
}

/// Records what the listing said and what it did, replacing whatever a previous lookup left,
/// so an artifact carries one lookup row per field rather than one per pass.
fn record_listing(transaction: &RwTransaction<'_>, facts: ListingFacts<'_>) -> Result<()> {
    let ListingFacts {
        id,
        listed,
        refinement,
        held,
        trigger,
    } = facts;
    let mut detail: ArtifactDetail = transaction
        .get()
        .primary(id)?
        .unwrap_or_else(|| ArtifactDetail::new(id, None, Vec::new()));
    let value = match refinement {
        Refinement::Refined { base, .. } | Refinement::Named { base, .. } => base.to_string(),
        Refinement::Contradicted { listed } => listed.to_string(),
        Refinement::Unchanged => listed.base_model.clone(),
    };
    let mut detail_text = format!(
        "Civitai lists these bytes as {} {}, base {}",
        listed.model, listed.version, listed.base_model
    );
    match refinement {
        Refinement::Contradicted { .. } => {
            detail_text.push_str(&format!(" - hoard has {held}, which stands"));
        }
        Refinement::Unchanged if listed_base(&listed.base_model).is_none() => {
            detail_text.push_str(" - not a base hoard knows");
        }
        Refinement::Unchanged | Refinement::Refined { .. } | Refinement::Named { .. } => {}
    }
    detail
        .evidence
        .retain(|evidence| !is_a_lookup_on_the_base(evidence));
    detail.evidence.push(
        Evidence::new(
            LOOKUP_FIELD,
            value,
            EvidenceSource::Lookup,
            EvidenceSource::Lookup.weight(),
        )
        .with_detail(detail_text),
    );
    if let Some(words) = trigger {
        detail
            .evidence
            .retain(|evidence| !is_a_lookup_on_the_trigger(evidence));
        detail.evidence.push(
            Evidence::new(
                TRIGGER_FIELD,
                words,
                EvidenceSource::Lookup,
                EvidenceSource::Lookup.weight(),
            )
            .with_detail(format!(
                "Civitai lists the trigger words of {} {}",
                listed.model, listed.version
            )),
        );
    }
    detail.listing = Some(listed.clone());
    transaction.upsert(detail)?;
    Ok(())
}
