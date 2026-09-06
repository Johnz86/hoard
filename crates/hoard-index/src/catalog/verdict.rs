use serde::Serialize;

use hoard_core::{ArtifactId, Base, Role, SubRole};

use crate::config::IndexConfig;
use crate::error::{IndexError, Result};
use crate::triage::check_verdict;

use super::Catalog;
use super::models::{ArtifactRecord, Override};
use super::scan::{Counters, log_changes};

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
        let mut counters =
            Counters::load(&self.database, IndexConfig::default().triage_confidence)?;
        let transaction = self.database.rw_transaction()?;
        let mut record = require_record(&transaction, id)?;
        counters.touch(&transaction, id.get())?;
        apply_override(&mut record, &verdict);
        record.triage_cleared = true;
        transaction.upsert(record)?;
        transaction.upsert(verdict)?;
        counters.pending.push(id.get());
        log_changes(&transaction, &mut counters)?;
        transaction.commit()?;
        self.summarise(&counters)?;
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
    pub fn clear_triage(&self, id: ArtifactId) -> Result<()> {
        let mut counters =
            Counters::load(&self.database, IndexConfig::default().triage_confidence)?;
        let transaction = self.database.rw_transaction()?;
        let mut record = require_record(&transaction, id)?;
        counters.touch(&transaction, id.get())?;
        record.triage_cleared = true;
        transaction.upsert(record)?;
        counters.pending.push(id.get());
        log_changes(&transaction, &mut counters)?;
        transaction.commit()?;
        self.summarise(&counters)?;
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
        record.artifact.role = sub_role.role();
        record.artifact.role_confidence = 1.0;
    }
    if let Some(base) = &verdict.base {
        record.artifact.base_label = Some(base.as_str().to_owned());
        record.artifact.base = base.clone();
        record.artifact.base_confidence = 1.0;
    }
}
