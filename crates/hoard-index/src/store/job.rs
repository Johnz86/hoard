use std::fmt;
use std::str::FromStr;

use jiff::Timestamp;
use rusqlite::{Connection, Row, params};
use serde::Serialize;

use hoard_core::JobId;

use crate::error::{IndexError, Result};
use crate::store::convert::{
    collect_rows, now, parse_enum, parse_opt_timestamp, parse_timestamp, required_row,
    timestamp_text, to_u64,
};

const JOB_COLUMNS: &str = "id, kind, state, phase, subject, progress, bytes, total, rate_bps, \
     error, started, finished";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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

/// A scan, hash run, pull or apply as the JOBS tab shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JobRow {
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

impl JobRow {
    fn from_row(row: &Row<'_>) -> Result<Self> {
        Ok(Self {
            id: JobId::new(row.get("id")?),
            kind: row.get("kind")?,
            state: parse_enum(&row.get::<_, String>("state")?)?,
            phase: row.get("phase")?,
            subject: row.get("subject")?,
            progress: row.get::<_, f64>("progress")? as f32,
            bytes: to_u64(row.get::<_, i64>("bytes")?),
            total: row.get::<_, Option<i64>>("total")?.map(to_u64),
            rate_bps: row.get::<_, Option<i64>>("rate_bps")?.map(to_u64),
            error: row.get("error")?,
            started: parse_timestamp(&row.get::<_, String>("started")?)?,
            finished: parse_opt_timestamp(row.get::<_, Option<String>>("finished")?)?,
        })
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

pub fn create_job(conn: &Connection, kind: &str, subject: Option<&str>) -> Result<JobId> {
    let id = conn.query_row(
        "INSERT INTO job(kind, state, subject, started) VALUES(?1, ?2, ?3, ?4) RETURNING id",
        (
            kind,
            JobState::Running.as_str(),
            subject,
            timestamp_text(now()),
        ),
        |row| row.get::<_, i64>(0),
    )?;
    Ok(JobId::new(id))
}

pub fn update_job(conn: &Connection, id: JobId, progress: &JobProgress) -> Result<()> {
    conn.execute(
        "UPDATE job SET phase = ?2, progress = ?3, bytes = ?4, total = ?5, rate_bps = ?6 \
         WHERE id = ?1 AND state = 'running'",
        params![
            id.get(),
            progress.phase,
            f64::from(progress.progress),
            progress.bytes as i64,
            progress.total.map(|value| value as i64),
            progress.rate_bps.map(|value| value as i64),
        ],
    )?;
    Ok(())
}

pub fn finish_job(conn: &Connection, id: JobId) -> Result<()> {
    settle(conn, id, JobState::Done, None)
}

pub fn fail_job(conn: &Connection, id: JobId, error: &str) -> Result<()> {
    settle(conn, id, JobState::Failed, Some(error))
}

pub fn cancel_job(conn: &Connection, id: JobId) -> Result<()> {
    settle(conn, id, JobState::Cancelled, None)
}

fn settle(conn: &Connection, id: JobId, state: JobState, error: Option<&str>) -> Result<()> {
    let updated = conn.execute(
        "UPDATE job SET state = ?2, error = ?3, finished = ?4, \
             progress = CASE WHEN ?2 = 'done' THEN 1.0 ELSE progress END \
         WHERE id = ?1",
        (id.get(), state.as_str(), error, timestamp_text(now())),
    )?;
    if updated == 0 {
        return Err(IndexError::not_found("job", id.to_string()));
    }
    Ok(())
}

pub fn job(conn: &Connection, id: JobId) -> Result<JobRow> {
    required_row(
        conn.query_row(
            &format!("SELECT {JOB_COLUMNS} FROM job WHERE id = ?1"),
            [id.get()],
            |row| Ok(JobRow::from_row(row)),
        ),
        "job",
        id,
    )
}

/// Lists jobs newest first, optionally narrowed to one state.
pub fn list_jobs(conn: &Connection, state: Option<JobState>) -> Result<Vec<JobRow>> {
    match state {
        Some(state) => {
            let mut statement = conn.prepare(&format!(
                "SELECT {JOB_COLUMNS} FROM job WHERE state = ?1 ORDER BY id DESC"
            ))?;
            let rows = statement.query_map([state.as_str()], |row| Ok(JobRow::from_row(row)))?;
            collect_rows(rows)
        }
        None => {
            let mut statement =
                conn.prepare(&format!("SELECT {JOB_COLUMNS} FROM job ORDER BY id DESC"))?;
            let rows = statement.query_map([], |row| Ok(JobRow::from_row(row)))?;
            collect_rows(rows)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    #[test]
    fn a_job_reports_progress_then_finishes() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");
        let id = create_job(&conn, "scan", Some("cache")).expect("job starts");

        update_job(
            &conn,
            id,
            &JobProgress {
                phase: Some("hashing".to_owned()),
                progress: 0.5,
                bytes: 2048,
                total: Some(4096),
                rate_bps: Some(1024),
            },
        )
        .expect("job updates");
        let running = job(&conn, id).expect("job reads");
        assert_eq!(running.state, JobState::Running);
        assert_eq!(running.phase.as_deref(), Some("hashing"));
        assert_eq!(running.progress, 0.5);
        assert_eq!(running.total, Some(4096));
        assert!(running.finished.is_none());

        finish_job(&conn, id).expect("job finishes");
        let done = job(&conn, id).expect("job reads");
        assert_eq!(done.state, JobState::Done);
        assert_eq!(done.progress, 1.0);
        assert!(done.finished.is_some());

        update_job(&conn, id, &JobProgress::default()).expect("late ticks are ignored");
        assert_eq!(job(&conn, id).expect("job reads").progress, 1.0);
    }

    #[test]
    fn failure_and_cancellation_are_recorded_and_listed() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");
        let failed = create_job(&conn, "pull", None).expect("job starts");
        let cancelled = create_job(&conn, "apply", None).expect("job starts");
        let running = create_job(&conn, "scan", None).expect("job starts");

        fail_job(&conn, failed, "peer went offline").expect("job fails");
        cancel_job(&conn, cancelled).expect("job cancels");

        assert_eq!(
            job(&conn, failed).expect("job reads").error.as_deref(),
            Some("peer went offline")
        );
        assert!(job(&conn, cancelled).expect("job reads").state.is_final());
        assert_eq!(list_jobs(&conn, None).expect("jobs list").len(), 3);
        assert_eq!(
            list_jobs(&conn, Some(JobState::Running)).expect("jobs list")[0].id,
            running
        );
        assert_eq!(
            list_jobs(&conn, Some(JobState::Failed))
                .expect("jobs list")
                .len(),
            1
        );
        assert!(fail_job(&conn, JobId::new(404), "gone").is_err());
    }

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
        }
        assert!("paused".parse::<JobState>().is_err());
    }
}
