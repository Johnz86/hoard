use super::*;

#[test]
fn usage_is_logged_summarised_and_reaches_the_used_token() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (_input, config) = loose_fixture(directory.path())?;
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    catalog.rebuild_search(|_| {})?;
    let row = search(&catalog, "role:lora")?.rows[0].clone();
    let repo = row.repo_id;
    let now = jiff::Timestamp::now();
    for client in ["a", "b", "b"] {
        catalog.log_usage(&UsageEvent {
            client: Some(client.to_owned()),
            repo: Some(repo),
            artifact: Some(row.artifact_id),
            bytes: 10,
            rate_bps: Some(5),
            ..UsageEvent::new("keep.safetensors", now)
        })?;
    }
    catalog.bump_artifact_use(row.artifact_id, now)?;
    assert_eq!(catalog.distinct_clients(repo)?, 2);
    assert_eq!(catalog.recent_pulls(repo, 10)?.len(), 3);
    let days = catalog.usage_days(repo, 7)?;
    assert_eq!(days.len(), 7);
    assert_eq!(days[6].pulls, 3, "today is the last bucket: {days:?}");
    let usage = catalog.artifact_usage(row.artifact_id)?;
    assert_eq!(usage.pulls, 1);
    assert!(usage.last_used.is_some());
    assert!(
        catalog.pending_changes()? > 0,
        "a bump logs the artifact for the index"
    );
    catalog.refresh_search(|_| {})?;
    assert_eq!(search(&catalog, "used:<1d")?.total, 1);
    assert_eq!(search(&catalog, "used:>30d")?.total, 0);
    assert_eq!(
        search(&catalog, "role:lora")?.rows[0].last_used,
        usage.last_used
    );
    let context = catalog.compile_context("", now, Vec::new())?;
    let by_use = catalog.query(QueryRequest {
        raw: "",
        sort: SortSpec::new(SortKey::Used, true),
        limit: 10,
        offset: 0,
        context: &context,
    })?;
    assert_eq!(
        by_use.rows[0].artifact_id, row.artifact_id,
        "the used artifact sorts first descending"
    );
    Ok(())
}

#[test]
fn a_usage_window_counts_only_the_days_it_shows() -> Result<()> {
    let (_directory, catalog, ids) = seeded_catalog()?;
    let row = catalog
        .artifact_row(ids.lora)?
        .ok_or_else(|| IndexError::Catalog("lora".into()))?;
    let midday = jiff::Timestamp::now()
        .to_zoned(jiff::tz::TimeZone::UTC)
        .date()
        .at(12, 0, 0, 0)
        .to_zoned(jiff::tz::TimeZone::UTC)
        .map_err(|error| IndexError::Catalog(format!("noon: {error}")))?
        .timestamp();
    let mut events = Vec::with_capacity(500);
    for index in 0..500_u32 {
        let at = midday - std::time::Duration::from_secs(u64::from(index % 40) * 86_400);
        events.push(UsageEvent {
            client: Some(format!("client{}", index % 3)),
            repo: Some(row.repo_id),
            artifact: Some(row.artifact_id),
            bytes: u64::from(index),
            ..UsageEvent::new("keep.safetensors", at)
        });
    }
    catalog.log_usage_batch(&events)?;

    let window = catalog.usage_days(row.repo_id, 7)?;
    assert_eq!(window.len(), 7);
    let inside: u64 = (0..500_u32).filter(|index| index % 40 < 7).count() as u64;
    assert_eq!(
        window.iter().map(|day| day.pulls).sum::<u64>(),
        inside,
        "only the seven days the chart draws are counted: {window:?}"
    );
    for (offset, day) in window.iter().rev().enumerate() {
        let expected = (0..500_u32)
            .filter(|index| usize::try_from(index % 40).unwrap_or(usize::MAX) == offset)
            .count() as u64;
        assert_eq!(day.pulls, expected, "day {offset} of the window");
    }

    let report = catalog.usage_report(row.repo_id, 7, 20)?;
    assert_eq!(report.days, window, "one load answers the same window");
    assert_eq!(report.clients, catalog.distinct_clients(row.repo_id)?);
    assert_eq!(report.pulls, catalog.recent_pulls(row.repo_id, 20)?);
    assert_eq!(report.pulls.len(), 20);
    Ok(())
}

#[test]
fn the_usage_report_reads_a_window_and_a_maintained_count() -> Result<()> {
    let (_directory, catalog, ids) = seeded_catalog()?;
    let row = catalog
        .artifact_row(ids.lora)?
        .ok_or_else(|| IndexError::Catalog("lora".into()))?;
    let midday = jiff::Timestamp::now()
        .to_zoned(jiff::tz::TimeZone::UTC)
        .date()
        .at(12, 0, 0, 0)
        .to_zoned(jiff::tz::TimeZone::UTC)
        .map_err(|error| IndexError::Catalog(format!("noon: {error}")))?
        .timestamp();
    let events: Vec<UsageEvent> = (0..2_000_u32)
        .map(|index| {
            let at = midday - std::time::Duration::from_secs(u64::from(index % 40) * 86_400);
            UsageEvent {
                client: Some(format!("client{}", index % 7)),
                repo: Some(row.repo_id),
                artifact: Some(row.artifact_id),
                bytes: u64::from(index),
                ..UsageEvent::new(format!("f{index}.safetensors"), at)
            }
        })
        .collect();
    catalog.log_usage_batch(&events)?;

    let inside = (0..2_000_u32).filter(|index| index % 40 < 30).count() as u64;
    let report = catalog.usage_report(row.repo_id, 30, 20)?;
    assert_eq!(report.days.len(), 30, "the window lists every day it draws");
    assert_eq!(
        report.days.iter().map(|day| day.pulls).sum::<u64>(),
        inside,
        "the window sums the events inside it: {:?}",
        report.days
    );
    assert_eq!(
        report.clients, 7,
        "the maintained record counts the clients"
    );
    assert_eq!(
        report.pulls.len(),
        20,
        "the newest rows are the limit the caller asked for"
    );
    assert_eq!(
        report.pulls,
        catalog.recent_pulls(row.repo_id, 20)?,
        "the report and the standalone reader answer the same rows"
    );
    assert!(
        report
            .pulls
            .windows(2)
            .all(|pair| pair[0].when >= pair[1].when),
        "the pulls are newest first: {:?}",
        report.pulls
    );
    assert_eq!(report.pulls[0].when, midday, "the newest row is today's");

    let touched = catalog.usage_in(row.repo_id, &Window::of(30)?)?.len() as u64;
    assert_eq!(
        touched, inside,
        "the range reads the rows inside the window, not the 2000 the log holds"
    );

    let second = midday + std::time::Duration::from_secs(60);
    let split = second + std::time::Duration::from_millis(500);
    catalog.log_usage_batch(&[
        UsageEvent {
            client: Some("client0".to_owned()),
            repo: Some(row.repo_id),
            ..UsageEvent::new("whole.safetensors", second)
        },
        UsageEvent {
            client: Some("client0".to_owned()),
            repo: Some(row.repo_id),
            ..UsageEvent::new("fractional.safetensors", split)
        },
    ])?;
    let newest = catalog.recent_pulls(row.repo_id, 2)?;
    assert_eq!(
        newest
            .iter()
            .map(|pull| pull.what.as_str())
            .collect::<Vec<_>>(),
        vec!["fractional.safetensors", "whole.safetensors"],
        "a fractional second sorts after the whole second it follows: {newest:?}"
    );
    assert_eq!(newest[0].when, split);
    Ok(())
}

#[test]
fn a_usage_batch_maintains_the_repo_record_in_one_transaction() -> Result<()> {
    let (_directory, catalog, ids) = seeded_catalog()?;
    let lora = catalog
        .artifact_row(ids.lora)?
        .ok_or_else(|| IndexError::Catalog("lora".into()))?;
    let ckpt = catalog
        .artifact_row(ids.ckpt)?
        .ok_or_else(|| IndexError::Catalog("ckpt".into()))?;
    let now = jiff::Timestamp::now();
    let earlier = now - std::time::Duration::from_secs(3_600);
    let event = |row: &SearchRow, client: &str, at: jiff::Timestamp| UsageEvent {
        client: Some(client.to_owned()),
        repo: Some(row.repo_id),
        artifact: Some(row.artifact_id),
        bytes: 10,
        ..UsageEvent::new(row.path.clone(), at)
    };

    catalog.log_usage_batch(&[
        event(&lora, "10.0.0.4", earlier),
        event(&lora, "10.0.0.5", now),
        event(&lora, "10.0.0.4", now),
        event(&ckpt, "10.0.0.9", earlier),
    ])?;

    let stored = catalog
        .repo_usage(lora.repo_id)?
        .ok_or_else(|| IndexError::Catalog("the batch wrote the record".into()))?;
    assert_eq!(stored.pulls, 3, "one batch counts every row it wrote");
    assert_eq!(
        stored.clients,
        vec!["10.0.0.4".to_owned(), "10.0.0.5".to_owned()],
        "the clients are held once each, sorted"
    );
    assert_eq!(stored.last_pull, now.to_string(), "the latest time wins");
    assert_eq!(
        catalog.repo_usage(ckpt.repo_id)?.map(|stored| stored.pulls),
        Some(1),
        "each repository the batch served has its own record"
    );

    catalog.log_usage_batch(&[event(&lora, "10.0.0.6", earlier)])?;
    let after = catalog
        .repo_usage(lora.repo_id)?
        .ok_or_else(|| IndexError::Catalog("the record survives a second batch".into()))?;
    assert_eq!(after.pulls, 4, "a later batch adds to the count");
    assert_eq!(catalog.distinct_clients(lora.repo_id)?, 3);
    assert_eq!(
        after.last_pull, stored.last_pull,
        "an older row does not move the last pull"
    );
    Ok(())
}

#[test]
fn a_usage_batch_writes_every_row_and_one_change_per_artifact() -> Result<()> {
    let (_directory, catalog, ids) = seeded_catalog()?;
    catalog.refresh_search(|_| {})?;
    assert_eq!(catalog.pending_changes()?, 0, "the log starts settled");
    let lora = catalog
        .artifact_row(ids.lora)?
        .ok_or_else(|| IndexError::Catalog("lora".into()))?;
    let ckpt = catalog
        .artifact_row(ids.ckpt)?
        .ok_or_else(|| IndexError::Catalog("ckpt".into()))?;
    let now = jiff::Timestamp::now();
    let earlier = now - std::time::Duration::from_secs(60);
    let event = |row: &SearchRow, at: jiff::Timestamp| UsageEvent {
        client: Some("10.0.0.4".to_owned()),
        repo: Some(row.repo_id),
        artifact: Some(row.artifact_id),
        bytes: 10,
        ..UsageEvent::new(row.path.clone(), at)
    };

    let written =
        catalog.log_usage_batch(&[event(&lora, earlier), event(&ckpt, now), event(&lora, now)])?;

    assert_eq!(written, 3, "every event is one row");
    assert_eq!(catalog.recent_pulls(lora.repo_id, 10)?.len(), 2);
    assert_eq!(catalog.recent_pulls(ckpt.repo_id, 10)?.len(), 1);
    let lora_usage = catalog.artifact_usage(ids.lora)?;
    assert_eq!(lora_usage.pulls, 2, "one bump carries both uses");
    assert_eq!(lora_usage.last_used, Some(now), "the latest time wins");
    assert_eq!(catalog.artifact_usage(ids.ckpt)?.pulls, 1);
    assert_eq!(
        catalog.pending_changes()?,
        2,
        "one search change per distinct artifact"
    );
    Ok(())
}

#[test]
fn the_usage_chart_is_read_off_the_repository_record_and_not_the_log() -> Result<()> {
    let (_directory, catalog, ids) = seeded_catalog()?;
    let row = catalog
        .artifact_row(ids.lora)?
        .ok_or_else(|| IndexError::Catalog("lora".into()))?;
    let now = jiff::Timestamp::now();
    for index in 0..3_u32 {
        catalog.log_usage(&UsageEvent {
            client: Some(format!("10.0.0.{index}")),
            repo: Some(row.repo_id),
            artifact: Some(row.artifact_id),
            ..UsageEvent::new("keep.safetensors", now)
        })?;
    }
    let charted = catalog.usage_days(row.repo_id, 7)?;
    assert_eq!(charted.len(), 7);
    assert_eq!(
        charted[6].pulls, 3,
        "today holds the three pulls: {charted:?}"
    );

    let transaction = catalog.database.rw_transaction()?;
    let logged: Vec<crate::catalog::models::Usage> = transaction
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    assert_eq!(logged.len(), 3);
    for entry in logged {
        transaction.remove(entry)?;
    }
    transaction.commit()?;

    assert_eq!(
        catalog.usage_days(row.repo_id, 7)?,
        charted,
        "the chart is the record's, not a pass over the log"
    );
    assert_eq!(
        catalog.usage_report(row.repo_id, 7, 20)?.days,
        charted,
        "and the report draws the same chart"
    );
    assert!(
        catalog.recent_pulls(row.repo_id, 20)?.is_empty(),
        "the pulls list is still the log's"
    );
    let longer = catalog.usage_days(row.repo_id, HISTORY_DAYS as u32 + 9)?;
    assert_eq!(
        longer.iter().map(|day| day.pulls).sum::<u64>(),
        0,
        "a window longer than the record keeps is counted from the log"
    );
    Ok(())
}

#[test]
fn a_retired_repository_forgets_more_pulls_than_one_transaction_holds() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir_all(input.join("loras"))?;
    fs::write(input.join("loras/style.safetensors"), lora_header())?;
    let config = IndexConfig {
        sources: vec![source(&input, "files")],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?;
    catalog.rebuild_search(|_| {})?;
    let row = search(&catalog, "role:lora")?
        .rows
        .into_iter()
        .next()
        .ok_or_else(|| IndexError::Catalog("the lora is indexed".into()))?;
    let now = jiff::Timestamp::now();
    let served: Vec<UsageEvent> = (0..crate::catalog::scan::CHUNK + 7)
        .map(|index| UsageEvent {
            client: Some(format!("10.0.0.{}", index % 8)),
            repo: Some(row.repo_id),
            artifact: Some(row.artifact_id),
            ..UsageEvent::new("style.safetensors", now)
        })
        .collect();
    catalog.log_usage_batch(&served)?;
    assert_eq!(
        catalog.usage_days(row.repo_id, 7)?[6].pulls,
        served.len() as u64
    );

    let dropped = IndexConfig {
        sources: Vec::new(),
        ..config
    };
    catalog.rescan(
        &dropped,
        HashPolicy::Never,
        RescanScope::Configured,
        |_, _| {},
    )?;

    assert!(catalog.sources()?.is_empty(), "the source is retired");
    let read = catalog.database.r_transaction()?;
    let left: Vec<crate::catalog::models::Usage> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    assert!(
        left.is_empty(),
        "every row of the retired repository's log went with it, {} left",
        left.len()
    );
    drop(read);
    assert_eq!(catalog.distinct_clients(row.repo_id)?, 0);
    assert!(catalog.recent_pulls(row.repo_id, 20)?.is_empty());
    Ok(())
}
