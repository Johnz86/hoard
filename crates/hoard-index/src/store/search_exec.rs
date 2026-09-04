use rusqlite::{Connection, params_from_iter};

use crate::error::{IndexError, Result};
use crate::store::convert::{collect_rows, to_u64};
use crate::store::{FacetBucket, SEARCH_COLUMNS, SearchRow, SqlValue};

/// The view columns a facet may group by. The search grammar names one of these; anything
/// else is rejected rather than interpolated.
pub const FACET_COLUMNS: [&str; 8] = [
    "role",
    "base",
    "format",
    "precision",
    "node",
    "state",
    "source",
    "repo",
];

const DEFAULT_ORDER: &str = "size DESC, artifact_id";

/// Runs a compiled query, collapsing the entries of one artifact to the row the reader
/// should see: this node first, then the state with the most bytes behind it.
pub fn query_rows(
    conn: &Connection,
    where_sql: &str,
    params: &[SqlValue],
    order_sql: &str,
    limit: u32,
    offset: u32,
) -> Result<Vec<SearchRow>> {
    let mut values = params.to_vec();
    values.push(SqlValue::Int(i64::from(limit)));
    values.push(SqlValue::Int(i64::from(offset)));
    let sql = format!(
        "SELECT {SEARCH_COLUMNS} FROM ( \
             SELECT {SEARCH_COLUMNS}, MAX(preference) AS preference FROM search_row \
             WHERE {} GROUP BY artifact_id \
         ) ORDER BY {} LIMIT ?{} OFFSET ?{}",
        filter(where_sql),
        order(order_sql),
        values.len() - 1,
        values.len(),
    );
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map(params_from_iter(values.iter()), |row| {
        Ok(SearchRow::from_row(row))
    })?;
    collect_rows(rows)
}

/// How many artifacts the query matches, which is the `total` the pager shows.
pub fn count_rows(conn: &Connection, where_sql: &str, params: &[SqlValue]) -> Result<u64> {
    let sql = format!(
        "SELECT COUNT(DISTINCT artifact_id) FROM search_row WHERE {}",
        filter(where_sql)
    );
    let count: i64 = conn.query_row(&sql, params_from_iter(params.iter()), |row| row.get(0))?;
    Ok(to_u64(count))
}

/// Counts artifacts per value of one facet dimension. `active` is left false: only the
/// caller knows which values the query already selects.
pub fn facet_counts(
    conn: &Connection,
    column: &str,
    where_sql: &str,
    params: &[SqlValue],
) -> Result<Vec<FacetBucket>> {
    if !FACET_COLUMNS.contains(&column) {
        return Err(IndexError::Query(format!(
            "unknown facet dimension {column:?}"
        )));
    }
    let sql = format!(
        "SELECT {column} AS value, COUNT(DISTINCT artifact_id) AS hits FROM search_row \
         WHERE ({}) AND {column} IS NOT NULL AND {column} <> '' \
         GROUP BY {column} ORDER BY hits DESC, value",
        filter(where_sql)
    );
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map(params_from_iter(params.iter()), |row| {
        Ok(FacetBucket {
            value: row.get(0)?,
            count: to_u64(row.get::<_, i64>(1)?),
            active: false,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<FacetBucket>>>()?)
}

/// How many nodes hold something the query matches, which is the `nodes` count.
pub fn distinct_node_count(conn: &Connection, where_sql: &str, params: &[SqlValue]) -> Result<u64> {
    let sql = format!(
        "SELECT COUNT(DISTINCT node_row_id) FROM search_row WHERE {}",
        filter(where_sql)
    );
    let count: i64 = conn.query_row(&sql, params_from_iter(params.iter()), |row| row.get(0))?;
    Ok(to_u64(count))
}

fn filter(where_sql: &str) -> &str {
    if where_sql.trim().is_empty() {
        "1"
    } else {
        where_sql
    }
}

fn order(order_sql: &str) -> &str {
    if order_sql.trim().is_empty() {
        DEFAULT_ORDER
    } else {
        order_sql
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::store::fixture::{Fixture, SHARED_SIZE};
    use hoard_core::{Role, State};

    #[test]
    fn an_artifact_on_two_nodes_collapses_to_the_local_row() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");
        let world = Fixture::build(&conn);
        world.add_peer_copy(&conn);

        let rows = query_rows(&conn, "", &[], "", 50, 0).expect("rows read");
        assert_eq!(rows.len(), 2);
        let shared = rows
            .iter()
            .find(|row| row.artifact_id == world.shared_artifact)
            .expect("the shared artifact is listed once");
        assert_eq!(shared.node, "workstation");
        assert_eq!(shared.state, State::Served);
        assert_eq!(shared.role, Role::Lora);
        assert_eq!(shared.size, SHARED_SIZE);
        assert!(shared.dupe);
        assert_eq!(shared.path_count, 3);
        assert_eq!(shared.repo, "models/org/name");
        assert_eq!(shared.revision, "main");
        assert_eq!(shared.source.as_deref(), Some("cache"));

        assert_eq!(
            count_rows(&conn, "", &[]).expect("count"),
            rows.len() as u64
        );
        assert_eq!(distinct_node_count(&conn, "", &[]).expect("nodes"), 2);
    }

    #[test]
    fn facets_agree_with_the_rows_the_same_query_returns() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");
        let world = Fixture::build(&conn);
        world.add_peer_copy(&conn);

        let roles = facet_counts(&conn, "role", "", &[]).expect("facets count");
        assert_eq!(roles.len(), 2);
        assert!(
            roles
                .iter()
                .all(|bucket| bucket.count == 1 && !bucket.active)
        );

        let nodes = facet_counts(&conn, "node", "", &[]).expect("facets count");
        assert_eq!(nodes[0].value, "workstation");
        assert_eq!(nodes[0].count, 2);
        assert_eq!(nodes[1].value, "laptop");
        assert_eq!(nodes[1].count, 1);

        let where_sql = "role = ?1";
        let params = [SqlValue::text("lora")];
        let rows = query_rows(&conn, where_sql, &params, "", 50, 0).expect("rows read");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].artifact_id, world.shared_artifact);
        assert_eq!(count_rows(&conn, where_sql, &params).expect("count"), 1);
        assert_eq!(
            facet_counts(&conn, "node", where_sql, &params)
                .expect("facets count")
                .len(),
            2
        );
        assert_eq!(
            distinct_node_count(&conn, where_sql, &params).expect("nodes"),
            2
        );
    }

    #[test]
    fn ordering_and_paging_walk_the_result_once() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.write().expect("writer");
        let world = Fixture::build(&conn);

        let first = query_rows(&conn, "", &[], "size DESC", 1, 0).expect("page reads");
        let second = query_rows(&conn, "", &[], "size DESC", 1, 1).expect("page reads");
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_eq!(first[0].artifact_id, world.shared_artifact);
        assert_eq!(second[0].artifact_id, world.other_artifact);
        assert!(
            query_rows(&conn, "", &[], "", 1, 2)
                .expect("page reads")
                .is_empty()
        );

        let ascending = query_rows(&conn, "", &[], "size ASC", 50, 0).expect("rows read");
        assert_eq!(ascending[0].artifact_id, world.other_artifact);
    }

    #[test]
    fn an_unknown_facet_dimension_is_refused() {
        let store = Store::open_in_memory().expect("store opens");
        let conn = store.read().expect("reader");
        let attack = "role) AS value, 1 FROM artifact WHERE (1";
        assert!(matches!(
            facet_counts(&conn, attack, "", &[]),
            Err(IndexError::Query(_))
        ));
        for column in FACET_COLUMNS {
            facet_counts(&conn, column, "", &[]).expect("every listed dimension works");
        }
    }
}
