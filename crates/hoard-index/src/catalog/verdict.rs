use serde::Serialize;

use hoard_core::{ArtifactId, Base, Role, State, SubRole};

use crate::error::{IndexError, Result};
use crate::triage::check_verdict;

use super::Catalog;
use super::models::{ArtifactRecord, Entry, LOCAL_NODE, Override};
use super::scan::{Counters, log_changes, paths_of_artifact};

/// A user's verdict on one artifact, as the routes that record and read it see it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OverrideRecord {
    pub artifact_id: ArtifactId,
    pub role: Option<Role>,
    pub sub_role: Option<SubRole>,
    pub base: Option<Base>,
    pub decided_at: String,
}

impl From<Override> for OverrideRecord {
    fn from(verdict: Override) -> Self {
        Self {
            artifact_id: ArtifactId::new(verdict.artifact_id),
            role: verdict.role,
            sub_role: verdict.sub_role,
            base: verdict.base,
            decided_at: verdict.decided_at,
        }
    }
}

#[derive(Debug)]
pub(super) enum Verdict {
    Confirmed(Override),
    Rejected,
    /// The rejection is taken back: the record wants a person again if anything about it is
    /// still open.
    Restored,
}

impl Catalog {
    /// Records a user's verdict, rewrites the artifact's role, sub-role and base, clears
    /// triage, and logs the artifact for the search index. Refreshes the index.
    ///
    /// A verdict that names a role settles it at full confidence, which takes the artifact
    /// out of the triage total, so the record is measured before and after and the totals
    /// moved by the difference. Every path the artifact holds is then settled from `triage`
    /// to `local` in the same transaction, as a rejection settles them. A record that still
    /// needs triage afterwards, an incomplete set, or a verdict that leaves the role unsettled
    /// (no role, or `unk`), keeps its paths in `triage`, which is what the next scan of those
    /// files would write.
    ///
    /// A role and a sub-role that disagree are refused rather than stored; see
    /// [`crate::triage::check_verdict`].
    pub fn put_override(
        &self,
        id: ArtifactId,
        role: Option<Role>,
        sub_role: Option<SubRole>,
        base: Option<Base>,
    ) -> Result<()> {
        check_verdict(role, sub_role)?;
        let verdict = Override {
            artifact_id: id.get(),
            role,
            sub_role,
            base,
            decided_at: jiff::Timestamp::now().to_string(),
        };
        self.record_verdict(id, Verdict::Confirmed(verdict))
    }

    /// Forgets the verdict; the artifact keeps what the verdict wrote until a rescan
    /// re-infers it, so nothing the totals count moves here. Returns whether one existed.
    pub fn clear_override(&self, id: ArtifactId) -> Result<bool> {
        let transaction = self.database.rw_transaction()?;
        let stored: Option<Override> = transaction.get().primary(id.get())?;
        let existed = match stored {
            Some(verdict) => {
                transaction.remove(verdict)?;
                true
            }
            None => false,
        };
        transaction.commit()?;
        Ok(existed)
    }

    pub fn artifact_override(&self, id: ArtifactId) -> Result<Option<OverrideRecord>> {
        let stored: Option<Override> = self.database.r_transaction()?.get().primary(id.get())?;
        Ok(stored.map(OverrideRecord::from))
    }

    /// Takes the artifact out of the triage queue without a verdict, which is what a
    /// rejection leaves behind.
    ///
    /// The record is marked rejected and every path it still holds is settled from `triage`
    /// to `local` in the same transaction, so the queue, the `state:triage` shelf and the
    /// stats total drop together rather than one of them at a time. An incomplete set leaves
    /// the queue this way or by its missing files landing, and no other.
    pub fn clear_triage(&self, id: ArtifactId) -> Result<()> {
        self.record_verdict(id, Verdict::Rejected)
    }

    pub(super) fn record_verdict(&self, id: ArtifactId, verdict: Verdict) -> Result<()> {
        let threshold = self.stored_triage()?;
        let mut counters = Counters::load(&self.database, threshold)?;
        let transaction = self.database.rw_transaction()?;
        let mut record = require_record(&transaction, id)?;
        counters.touch(&transaction, id.get())?;
        let restored = match &verdict {
            Verdict::Confirmed(over) => {
                apply_override(&mut record, over);
                false
            }
            Verdict::Rejected => {
                record.rejected = true;
                false
            }
            Verdict::Restored => {
                record.rejected = false;
                true
            }
        };
        let settled = !record.needs_triage(threshold);
        transaction.upsert(record)?;
        if let Verdict::Confirmed(over) = verdict {
            transaction.upsert(over)?;
        }
        if settled {
            leave_triage(&transaction, &mut counters, id.get())?;
        } else if restored {
            enter_triage(&transaction, &mut counters, id.get())?;
        }
        counters.pending.push(id.get());
        log_changes(&transaction, &mut counters)?;
        counters.save(&transaction)?;
        transaction.commit()?;
        self.refresh_search(|_| {})?;
        Ok(())
    }
}

fn leave_triage(
    transaction: &native_db::transaction::RwTransaction<'_>,
    counters: &mut Counters,
    id: i64,
) -> Result<()> {
    for entry in paths_of_artifact(transaction, id)? {
        if !entry.triage && entry.state != State::Triage {
            continue;
        }
        counters.derived.removed(&entry);
        let settled = Entry {
            state: match entry.state {
                State::Triage => State::Local,
                State::Local | State::Served | State::Stale | State::Pulling | State::Remote => {
                    entry.state
                }
            },
            triage: false,
            ..entry
        };
        counters.derived.added(&settled);
        transaction.upsert(settled)?;
    }
    Ok(())
}

/// The mirror of [`leave_triage`]: every path this node holds settled goes back to `triage`,
/// so an artifact whose rejection was taken back is queued, counted and shelved again at
/// once. A served path keeps the mark the mirror gave it and only takes the flag, exactly as
/// [`leave_triage`] leaves it served when it clears the flag. A path another node
/// advertises, one being pulled and one already in triage are left as they are.
fn enter_triage(
    transaction: &native_db::transaction::RwTransaction<'_>,
    counters: &mut Counters,
    id: i64,
) -> Result<()> {
    for entry in paths_of_artifact(transaction, id)? {
        if entry.node != LOCAL_NODE {
            continue;
        }
        let state = match entry.state {
            State::Local => State::Triage,
            State::Served => State::Served,
            State::Triage | State::Stale | State::Pulling | State::Remote => continue,
        };
        counters.derived.removed(&entry);
        let waiting = Entry {
            state,
            triage: true,
            ..entry
        };
        counters.derived.added(&waiting);
        transaction.upsert(waiting)?;
    }
    Ok(())
}

fn require_record(
    transaction: &native_db::transaction::RwTransaction<'_>,
    id: ArtifactId,
) -> Result<ArtifactRecord> {
    transaction
        .get()
        .primary(id.get())?
        .ok_or_else(|| IndexError::not_found("artifact", id.to_string()))
}

/// Writes a verdict over what inference decided. Every field the verdict names reaches
/// confidence 1.0, and a sub-role carries the role it belongs to.
pub(super) fn apply_override(record: &mut ArtifactRecord, verdict: &Override) {
    if let Some(role) = verdict.role {
        record.artifact.role = role;
        record.artifact.role_confidence = 1.0;
    }
    if let Some(sub_role) = verdict.sub_role {
        record.artifact.sub_role = Some(sub_role);
        record.artifact.role = sub_role.role_with(record.artifact.role);
        record.artifact.role_confidence = 1.0;
    }
    if let Some(base) = &verdict.base {
        record.artifact.base_label = Some(base.as_str().to_owned());
        record.artifact.base = base.clone();
        record.artifact.base_confidence = 1.0;
    }
}
