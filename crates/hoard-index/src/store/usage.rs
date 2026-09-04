use jiff::Timestamp;
use rusqlite::{Connection, Row, params};
use serde::Serialize;

use hoard_core::{ArtifactId, RepoRowId};

use crate::error::Result;
use crate::store::convert::{collect_rows, parse_timestamp, timestamp_text, to_u64};

/// One served request, which is what the USAGE tab and `used:` queries read.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageEvent {
    pub at: Timestamp,
    pub client: Option<String>,
    pub repo: Option<RepoRowId>,
    pub artifact: Option<ArtifactId>,
    pub path: String,
    pub bytes: u64,
    pub rate_bps: Option<u64>,
    pub status: u16,
}

impl UsageEvent {
    pub fn new(path: impl Into<String>, at: Timestamp) -> Self {
        Self {
            at,
            client: None,
            repo: None,
            artifact: None,
            path: path.into(),
            bytes: 0,
            rate_bps: None,
            status: 200,
        }
    }
}

/// One bar of the seven day usage chart.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UsageDay {
    pub date: String,
    pub pulls: u64,
}

/// One line of the RECENT PULLS list.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PullRow {
    pub when: Timestamp,
    pub who: Option<String>,
    pub what: String,
    pub bytes: u64,
    pub rate_bps: Option<u64>,
}

impl PullRow {
    fn from_row(row: &Row<'_>) -> Result<Self> {
        Ok(Self {
            when: parse_timestamp(&row.get::<_, String>("at")?)?,
            who: row.get("client")?,
            what: row.get("path")?,
            bytes: to_u64(row.get::<_, i64>("bytes")?),
            rate_bps: row.get::<_, Option<i64>>("rate_bps")?.map(to_u64),
        })
    }
}

pub fn log_usage(conn: &Connection, event: &UsageEvent) -> Result<i64> {
    let id = conn.query_row(
        "INSERT INTO usage(at, client, repo_id, artifact_id, path, bytes, rate_bps, status) \
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) RETURNING id",
        params![
            timestamp_text(event.at),
            event.client,
            event.repo.map(RepoRowId::get),
            event.artifact.map(ArtifactId::get),
            event.path,
            event.bytes as i64,
            event.rate_bps.map(|value| value as i64),
            i64::from(event.status),
        ],
        |row| row.get(0),
    )?;
    Ok(id)
}

/// Pull counts per day for the last `days` days, ending today and including empty days.
pub fn usage_days(conn: &Connection, repo: RepoRowId, days: u32) -> Result<Vec<UsageDay>> {
    let span = format!("-{} days", days.saturating_sub(1));
    let mut statement = conn.prepare(
        "WITH RECURSIVE window(day) AS ( \
             SELECT date('now', ?1) \
             UNION ALL SELECT date(day, '+1 day') FROM window WHERE day < date('now') \
         ) \
         SELECT window.day, ( \
             SELECT COUNT(*) FROM usage u \
             WHERE u.repo_id = ?2 AND substr(u.at, 1, 10) = window.day \
         ) FROM window ORDER BY window.day",
    )?;
    let rows = statement.query_map((span, repo.get()), |row| {
        Ok((|| {
            Ok(UsageDay {
                date: row.get::<_, String>(0)?,
                pulls: to_u64(row.get::<_, i64>(1)?),
            })
        })())
    })?;
    collect_rows(rows)
}

pub fn recent_pulls(conn: &Connection, repo: RepoRowId, limit: u32) -> Result<Vec<PullRow>> {
    let mut statement = conn.prepare(
        "SELECT at, client, path, bytes, rate_bps FROM usage WHERE repo_id = ?1 \
         ORDER BY at DESC, id DESC LIMIT ?2",
    )?;
    let rows = statement.query_map((repo.get(), limit), |row| Ok(PullRow::from_row(row)))?;
    collect_rows(rows)
}

/// How many different clients pulled from a repository, which is the `clients` count.
pub fn distinct_clients(conn: &Connection, repo: RepoRowId) -> Result<u64> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(DISTINCT client) FROM usage WHERE repo_id = ?1 AND client IS NOT NULL",
        [repo.get()],
        |row| row.get(0),
    )?;
    Ok(to_u64(count))
}

/// Records that an artifact was served, which is what `used:` and `last used` read.
pub fn bump_artifact_use(conn: &Connection, artifact: ArtifactId, at: Timestamp) -> Result<()> {
    conn.execute(
        "UPDATE artifact SET last_used = ?2, use_count = use_count + 1 WHERE id = ?1",
        (artifact.get(), timestamp_text(at)),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::store::artifact::artifact;
    use crate::store::fixture::Fixture;

    fn event(world: &Fixture, client: &str, at: Timestamp) -> UsageEvent {
        UsageEvent {
            client: Some(client.to_owned()),
            repo: Some(world.repo),
            artifact: Some(world.shared_artifact),
            bytes: 239_075_328,
            rate_bps: Some(112_000_000),
            ..UsageEvent::new("unet/model.safetensors", at)
        }
    }

    #[test]
    fn served_requests_feed_the_usage_tab() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");
        let world = Fixture::build(&conn);
        let today = Timestamp::now();
        let long_ago = today - jiff::Span::new().hours(24 * 30);

        log_usage(&conn, &event(&world, "comfyui", today)).expect("usage logs");
        log_usage(&conn, &event(&world, "llama-server", today)).expect("usage logs");
        log_usage(&conn, &event(&world, "comfyui", long_ago)).expect("usage logs");

        let days = usage_days(&conn, world.repo, 7).expect("days count");
        assert_eq!(days.len(), 7);
        assert_eq!(days.iter().map(|day| day.pulls).sum::<u64>(), 2);
        assert_eq!(days[6].pulls, 2);

        let pulls = recent_pulls(&conn, world.repo, 2).expect("pulls list");
        assert_eq!(pulls.len(), 2);
        assert_eq!(pulls[0].what, "unet/model.safetensors");
        assert_eq!(pulls[0].bytes, 239_075_328);
        assert_eq!(distinct_clients(&conn, world.repo).expect("clients"), 2);
    }

    #[test]
    fn serving_an_artifact_bumps_its_last_use() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");
        let world = Fixture::build(&conn);
        let at: Timestamp = "2026-09-04T10:00:00Z".parse().expect("timestamp");

        bump_artifact_use(&conn, world.shared_artifact, at).expect("use bumps");
        bump_artifact_use(&conn, world.shared_artifact, at).expect("use bumps");
        let row = artifact(&conn, world.shared_artifact).expect("artifact reads");
        assert_eq!(row.use_count, 2);
        assert_eq!(row.last_used, Some(at));
    }

    #[test]
    fn a_repository_without_traffic_still_reports_seven_days() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");
        let world = Fixture::build(&conn);
        let days = usage_days(&conn, world.repo, 7).expect("days count");
        assert_eq!(days.len(), 7);
        assert!(days.iter().all(|day| day.pulls == 0));
        assert_eq!(distinct_clients(&conn, world.repo).expect("clients"), 0);
        assert!(
            recent_pulls(&conn, world.repo, 10)
                .expect("pulls list")
                .is_empty()
        );
    }
}
