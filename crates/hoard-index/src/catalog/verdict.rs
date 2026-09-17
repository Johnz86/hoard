use serde::Serialize;

use hoard_core::{ArtifactId, Base, Role, State, SubRole};

use crate::error::{IndexError, Result};
use crate::triage::check_verdict;

use super::Catalog;
use super::models::{ArtifactRecord, Entry, Override};
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

impl Catalog {
    /// Records a user's verdict, rewrites the artifact's role, sub-role and base, clears
    /// triage, and logs the artifact for the search index. Refreshes the index.
    ///
    /// A verdict settles a role at full confidence, which takes the artifact out of the
    /// triage total, so the record is measured before and after and the totals moved by the
    /// difference.
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
        let mut counters = Counters::load(&self.database, self.stored_triage()?)?;
        let transaction = self.database.rw_transaction()?;
        let mut record = require_record(&transaction, id)?;
        counters.touch(&transaction, id.get())?;
        apply_override(&mut record, &verdict);
        record.triage_cleared = true;
        transaction.upsert(record)?;
        transaction.upsert(verdict)?;
        counters.pending.push(id.get());
        log_changes(&transaction, &mut counters)?;
        counters.settle_summary(&transaction)?;
        transaction.commit()?;
        self.refresh_search(|_| {})?;
        Ok(())
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
        let mut counters = Counters::load(&self.database, self.stored_triage()?)?;
        let transaction = self.database.rw_transaction()?;
        let mut record = require_record(&transaction, id)?;
        counters.touch(&transaction, id.get())?;
        record.triage_cleared = true;
        record.rejected = true;
        transaction.upsert(record)?;
        for entry in paths_of_artifact(&transaction, id.get())? {
            if !entry.triage && entry.state != State::Triage {
                continue;
            }
            counters.derived.removed(&entry);
            let settled = Entry {
                state: match entry.state {
                    State::Triage => State::Local,
                    State::Local
                    | State::Served
                    | State::Stale
                    | State::Pulling
                    | State::Remote => entry.state,
                },
                triage: false,
                ..entry
            };
            counters.derived.added(&settled);
            transaction.upsert(settled)?;
        }
        counters.pending.push(id.get());
        log_changes(&transaction, &mut counters)?;
        counters.save(&transaction)?;
        transaction.commit()?;
        self.refresh_search(|_| {})?;
        Ok(())
    }
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
