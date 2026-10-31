use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use jiff::Timestamp;
use native_db::transaction::RwTransaction;
use serde::{Deserialize, Serialize};

use hoard_core::JobId;

use crate::error::{IndexError, Result};
use crate::time::{now, parse_opt_timestamp, parse_timestamp, timestamp_text};

use super::Catalog;
use super::models::{Job, Setting};
use super::userdata::{Sequenced, next_id};

const JOB_HIGH_WATER: &str = "jobs.high_water";

fn job_high_water(transaction: &RwTransaction<'_>) -> Result<i64> {
    let stored: Option<Setting> = transaction.get().primary(JOB_HIGH_WATER.to_owned())?;
    stored
        .map(|stored| stored.value.parse::<i64>())
        .transpose()
        .map_err(|error| IndexError::Catalog(format!("invalid job sequence: {error}")))
        .map(|value| value.unwrap_or_default())
}

fn preserve_job_sequence(transaction: &RwTransaction<'_>) -> Result<()> {
    let high_water = job_high_water(transaction)?.max(next_id::<Job>(transaction)? - 1);
    transaction.upsert(Setting {
        key: JOB_HIGH_WATER.to_owned(),
        value: high_water.to_string(),
    })?;
    Ok(())
}

/// Where a job has got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Running,
    Done,
    Failed,
    Cancelled,
}

impl JobState {
    pub const fn as_str(self) -> &'static str {
        match self {
            JobState::Running => "running",
            JobState::Done => "done",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
        }
    }

    /// True once the job has stopped, whatever the outcome.
    pub const fn is_final(self) -> bool {
        !matches!(self, JobState::Running)
    }
}

impl fmt::Display for JobState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for JobState {
    type Err = IndexError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "running" => Ok(JobState::Running),
            "done" => Ok(JobState::Done),
            "failed" => Ok(JobState::Failed),
            "cancelled" => Ok(JobState::Cancelled),
            other => Err(IndexError::Config(format!("unknown job state {other:?}"))),
        }
    }
}

/// What a running job reports on each tick.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JobProgress {
    pub phase: Option<String>,
    pub progress: f32,
    pub bytes: u64,
    pub total: Option<u64>,
    pub rate_bps: Option<u64>,
}

/// A scan, hash run, pull or apply as the Sources activity section shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JobRecord {
    pub id: JobId,
    pub kind: String,
    pub state: JobState,
    pub phase: Option<String>,
    pub subject: Option<String>,
    pub progress: f32,
    pub bytes: u64,
    pub total: Option<u64>,
    pub rate_bps: Option<u64>,
    pub error: Option<String>,
    pub started: Timestamp,
    pub finished: Option<Timestamp>,
}

impl TryFrom<Job> for JobRecord {
    type Error = IndexError;

    fn try_from(stored: Job) -> Result<Self> {
        Ok(Self {
            id: JobId::new(stored.id),
            kind: stored.kind,
            state: stored.state,
            phase: stored.phase,
            subject: stored.subject,
            progress: stored.progress,
            bytes: stored.bytes,
            total: stored.total,
            rate_bps: stored.rate_bps,
            error: stored.error,
            started: parse_timestamp(&stored.started)?,
            finished: parse_opt_timestamp(stored.finished)?,
        })
    }
}

impl Sequenced for Job {
    fn sequence(&self) -> u64 {
        u64::try_from(self.id).unwrap_or_default()
    }
}

impl Catalog {
    pub fn create_job(&self, kind: &str, subject: Option<&str>) -> Result<JobId> {
        let transaction = self.database.rw_transaction()?;
        let high_water = job_high_water(&transaction)?;
        let next = high_water
            .checked_add(1)
            .ok_or_else(|| IndexError::Catalog("job sequence exhausted".into()))?;
        let id = next_id::<Job>(&transaction)?.max(next);
        transaction.upsert(Setting {
            key: JOB_HIGH_WATER.to_owned(),
            value: id.to_string(),
        })?;
        transaction.insert(Job {
            id,
            kind: kind.to_owned(),
            state: JobState::Running,
            phase: None,
            subject: subject.map(str::to_owned),
            progress: 0.0,
            bytes: 0,
            total: None,
            rate_bps: None,
            error: None,
            started: timestamp_text(now()),
            finished: None,
        })?;
        transaction.commit()?;
        Ok(JobId::new(id))
    }

    /// Records what a running job reports on one tick. A job that has already stopped keeps
    /// what it settled with, so a late tick cannot move a finished bar.
    pub fn update_job(&self, id: JobId, progress: &JobProgress) -> Result<()> {
        let transaction = self.database.rw_transaction()?;
        let stored: Option<Job> = transaction.get().primary(id.get())?;
        if let Some(stored) = stored.filter(|stored| stored.state == JobState::Running) {
            transaction.upsert(Job {
                phase: progress.phase.clone(),
                progress: progress.progress,
                bytes: progress.bytes,
                total: progress.total,
                rate_bps: progress.rate_bps,
                ..stored
            })?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn finish_job(&self, id: JobId) -> Result<()> {
        self.settle(id, JobState::Done, None)
    }

    pub fn fail_job(&self, id: JobId, error: &str) -> Result<()> {
        self.settle(id, JobState::Failed, Some(error))
    }

    pub fn cancel_job(&self, id: JobId) -> Result<()> {
        self.settle(id, JobState::Cancelled, None)
    }

    pub(crate) fn update_scan_subjects(&self, subjects: &[(JobId, String)]) -> Result<()> {
        for batch in subjects.chunks(super::scan::CHUNK) {
            let transaction = self.database.rw_transaction()?;
            let mut changed = false;
            for (id, subject) in batch {
                if let Some(job) = transaction.get().primary::<Job>(id.get())?
                    && job.kind == "scan"
                    && job.subject.as_deref() != Some(subject.as_str())
                {
                    transaction.upsert(Job {
                        subject: Some(subject.clone()),
                        ..job
                    })?;
                    changed = true;
                }
            }
            if changed {
                transaction.commit()?;
            }
        }
        Ok(())
    }

    fn settle(&self, id: JobId, state: JobState, error: Option<&str>) -> Result<()> {
        let transaction = self.database.rw_transaction()?;
        let stored: Option<Job> = transaction.get().primary(id.get())?;
        let stored = stored.ok_or_else(|| IndexError::not_found("job", id.to_string()))?;
        transaction.upsert(settled(stored, state, error))?;
        transaction.commit()?;
        Ok(())
    }

    pub fn job(&self, id: JobId) -> Result<Option<JobRecord>> {
        let stored: Option<Job> = self.database.r_transaction()?.get().primary(id.get())?;
        stored.map(JobRecord::try_from).transpose()
    }

    /// Deletes only a finished scan record. Protected IDs remain owned by a scan lane even
    /// when their stored state was already changed to cancelled.
    pub fn remove_finished_scan_job(&self, id: JobId, protected: &BTreeSet<JobId>) -> Result<()> {
        let transaction = self.database.rw_transaction()?;
        let job: Job = transaction
            .get()
            .primary(id.get())?
            .ok_or_else(|| IndexError::not_found("job", id.to_string()))?;
        if job.kind != "scan" || !job.state.is_final() || protected.contains(&id) {
            return Err(IndexError::Query(format!(
                "job {id} is not a finished scan"
            )));
        }
        preserve_job_sequence(&transaction)?;
        transaction.remove(job)?;
        transaction.commit()?;
        Ok(())
    }

    /// Clears completed scan history in bounded transactions, preserving other job kinds
    /// and jobs still owned by the scan lane.
    pub fn clear_finished_scan_jobs(&self, protected: &BTreeSet<JobId>) -> Result<usize> {
        let read = self.database.r_transaction()?;
        let mut ids = Vec::new();
        for job in read.scan().primary::<Job>()?.all()? {
            let job = job?;
            if job.kind == "scan"
                && job.state.is_final()
                && !protected.contains(&JobId::new(job.id))
            {
                ids.push(job.id);
            }
        }
        drop(read);
        let mut removed = 0;
        for batch in ids.chunks(super::scan::CHUNK) {
            let transaction = self.database.rw_transaction()?;
            preserve_job_sequence(&transaction)?;
            for id in batch {
                if let Some(job) = transaction.get().primary::<Job>(*id)?
                    && job.kind == "scan"
                    && job.state.is_final()
                {
                    transaction.remove(job)?;
                    removed += 1;
                }
            }
            transaction.commit()?;
        }
        Ok(removed)
    }

    /// Every job, or every job in one state, newest first.
    pub fn list_jobs(&self, state: Option<JobState>) -> Result<Vec<JobRecord>> {
        let read = self.database.r_transaction()?;
        let mut rows: Vec<Job> = read
            .scan()
            .primary()?
            .all()?
            .collect::<native_db::db_type::Result<_>>()?;
        rows.sort_unstable_by_key(|job| std::cmp::Reverse(job.id));
        rows.into_iter()
            .filter(|job| state.is_none_or(|wanted| job.state == wanted))
            .map(JobRecord::try_from)
            .collect()
    }

    /// Fails every job still running, which is what a stopped process left behind.
    pub fn sweep_running_jobs(&self, error: &str) -> Result<usize> {
        let transaction = self.database.rw_transaction()?;
        let running: Vec<Job> = transaction
            .scan()
            .primary::<Job>()?
            .all()?
            .filter(|job| job.as_ref().is_ok_and(|job| job.state == JobState::Running))
            .collect::<native_db::db_type::Result<_>>()?;
        let swept = running.len();
        for job in running {
            transaction.upsert(settled(job, JobState::Failed, Some(error)))?;
        }
        transaction.commit()?;
        Ok(swept)
    }
}

/// A job as it stops: a completed one reads all the way across, and every outcome records
/// when it stopped.
fn settled(job: Job, state: JobState, error: Option<&str>) -> Job {
    Job {
        state,
        error: error.map(str::to_owned),
        progress: match state {
            JobState::Done => 1.0,
            JobState::Running | JobState::Failed | JobState::Cancelled => job.progress,
        },
        finished: Some(timestamp_text(now())),
        ..job
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_states_round_trip_their_spelling() {
        for state in [
            JobState::Running,
            JobState::Done,
            JobState::Failed,
            JobState::Cancelled,
        ] {
            assert_eq!(
                state.as_str().parse::<JobState>().expect("state parses"),
                state
            );
            assert_eq!(state.to_string(), state.as_str());
        }
        assert!("paused".parse::<JobState>().is_err());
        assert!(!JobState::Running.is_final());
        assert!(JobState::Cancelled.is_final());
    }

    #[test]
    fn scan_cleanup_protects_owned_jobs_and_keeps_ids_monotonic_across_reopens() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let config = crate::config::IndexConfig {
            data_dir: directory.path().to_path_buf(),
            ..crate::config::IndexConfig::default()
        };
        let catalog = Catalog::open_or_scan(&config, "test", |_, _| {})?;
        let running = catalog.create_job("scan", None)?;
        let other = catalog.create_job("pull", None)?;
        catalog.finish_job(other)?;
        let done = catalog.create_job("scan", None)?;
        catalog.finish_job(done)?;
        let protected = catalog.create_job("scan", None)?;
        catalog.cancel_job(protected)?;
        let transaction = catalog.database.rw_transaction()?;
        let sequence: Setting = transaction
            .get()
            .primary(JOB_HIGH_WATER.to_owned())?
            .ok_or_else(|| IndexError::Catalog("job sequence missing".into()))?;
        transaction.remove(sequence)?;
        transaction.commit()?;
        let protected_ids = BTreeSet::from([protected]);
        assert!(
            catalog
                .remove_finished_scan_job(running, &protected_ids)
                .is_err()
        );
        assert!(
            catalog
                .remove_finished_scan_job(other, &protected_ids)
                .is_err()
        );
        assert!(
            catalog
                .remove_finished_scan_job(protected, &protected_ids)
                .is_err()
        );
        assert_eq!(catalog.clear_finished_scan_jobs(&protected_ids)?, 1);
        assert!(catalog.job(done)?.is_none());
        assert!(catalog.job(running)?.is_some());
        assert!(catalog.job(other)?.is_some());
        catalog.remove_finished_scan_job(protected, &BTreeSet::new())?;
        drop(catalog);
        let reopened = Catalog::open_or_scan(&config, "test", |_, _| {})?;
        assert!(reopened.create_job("scan", None)? > protected);
        Ok(())
    }
}
