//! `hoard ls`: the same grammar the search bar and the JSON API use, on the terminal.

use anyhow::{Context as _, Result};
use jiff::Timestamp;

use hoard_core::format_size;
use hoard_index::search::{self, CompileContext, SortKey, SortSpec};
use hoard_index::store::search_exec::query_rows;
use hoard_index::store::shelf::list_shelves;

use super::node::{Context as CliContext, local_node_name};

pub async fn run(
    context: CliContext,
    query: Option<String>,
    json: bool,
    limit: u32,
    sort: String,
    ascending: bool,
) -> Result<()> {
    let CliContext { store, .. } = context;
    let raw = query.unwrap_or_default();
    let parsed = search::parse(&raw);
    for warning in &parsed.warnings {
        tracing::warn!(%warning, "query warning");
    }

    let key: SortKey = sort
        .parse()
        .map_err(|_| anyhow::anyhow!("unknown sort column: {sort}"))?;
    let node = local_node_name(&store).await.ok();
    let shelves = store
        .call(list_shelves)
        .await
        .context("reading shelves")?
        .into_iter()
        .map(|shelf| (shelf.name, shelf.query))
        .collect();

    let mut compile_context = CompileContext::new(Timestamp::now()).with_shelves(shelves);
    if let Some(node) = node {
        compile_context = compile_context.with_local_node(node);
    }
    let compiled = search::compile(&parsed, SortSpec::new(key, !ascending), &compile_context)
        .context("compiling the query")?;

    let rows = store
        .call(move |conn| {
            query_rows(
                conn,
                &compiled.where_sql,
                &compiled.params,
                &compiled.order_sql,
                limit,
                0,
            )
        })
        .await
        .context("running the query")?;

    if json {
        let out = serde_json::to_string_pretty(&rows).context("rendering json")?;
        tracing::info!("{out}");
        return Ok(());
    }
    render(&rows);
    Ok(())
}

/// The nine columns of the design's table, in the same order, sized for a terminal.
fn render(rows: &[hoard_index::store::SearchRow]) {
    let mut out = String::new();
    out.push_str(&format!(
        "{:<5} {:<38} {:<10} {:<13} {:<7} {:>10} {:<10} {:<12} {}\n",
        "ROLE", "NAME", "BASE", "FORMAT", "PREC", "SIZE", "REVISION", "NODE", "STATE"
    ));
    for row in rows {
        let name = match row.org.as_deref() {
            Some(org) => format!("{org}/{}", row.name),
            None => row.name.clone(),
        };
        out.push_str(&format!(
            "{:<5} {:<38} {:<10} {:<13} {:<7} {:>10} {:<10} {:<12} {}\n",
            row.role.code(),
            truncate(&name, 38),
            truncate(row.base.as_str(), 10),
            truncate(&row.format.to_string(), 13),
            truncate(row.precision.as_ref().map_or("-", |p| p.as_str()), 7),
            format_size(row.size),
            truncate(&row.revision, 10),
            truncate(&row.node, 12),
            row.state.label(),
        ));
    }
    out.push_str(&format!("\n{} artifacts\n", rows.len()));
    tracing::info!("{out}");
}

fn truncate(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_owned();
    }
    let kept: String = value.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}~")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_values_are_truncated_to_the_column_width() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("a-very-long-artifact-name", 10), "a-very-lo~");
        assert_eq!(truncate("exactlyten", 10), "exactlyten");
    }
}
