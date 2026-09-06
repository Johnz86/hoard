use jiff::Timestamp;
use serde::Serialize;

use hoard_core::JobId;

use crate::error::{IndexError, Result};
use crate::store::convert::{now, parse_opt_timestamp, parse_timestamp, timestamp_text};
use crate::store::job::{JobProgress, JobState};

use super::Catalog;
use super::models::Job;
use super::userdata::{Sequenced, next_id};

/// A scan, hash run, pull or apply as the JOBS tab shows it. This is the shape the jobs
/// routes answer with, so it serialises exactly like the row the SQLite index kept.
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
        let id = next_id::<Job>(&transaction)?;
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
