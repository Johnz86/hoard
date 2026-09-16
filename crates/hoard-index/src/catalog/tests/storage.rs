use super::*;

#[test]
fn storage_reports_every_table_and_compaction_keeps_the_data() -> Result<()> {
    let (directory, mut catalog) = fixture()?;
    catalog.rebuild_search(|_| {})?;
    catalog.compact()?;
    assert_eq!(search(&catalog, "")?.total, 2, "compaction loses nothing");
    let native = directory.path().join("native");
    drop(catalog);
    let report = Catalog::storage(&native)?;
    assert!(report.file_bytes > 0);
    assert!(report.allocated_bytes > 0);
    let names: Vec<&str> = report
        .tables
        .iter()
        .map(|table| table.name.as_str())
        .collect();
    assert!(
        names.iter().any(|name| name.starts_with("7_1_")),
        "the Entry table is listed: {names:?}"
    );
    assert!(
        names.iter().any(|name| name.starts_with("3_1_")),
        "the ArtifactRecord table is listed: {names:?}"
    );
    let entries = report
        .tables
        .iter()
        .find(|table| table.name.starts_with("7_1_id"))
        .map(|table| table.entries);
    assert_eq!(entries, Some(2));
    let reopened = Catalog::open(&native)?;
    assert_eq!(search(&reopened, "")?.total, 2);
    Ok(())
}

#[test]
fn storage_reports_current_table_payload_and_page_costs() -> Result<()> {
    let (directory, catalog) = fixture()?;
    catalog.rebuild_search(|_| {})?;
    let native = directory.path().join("native");
    drop(catalog);
    let report = Catalog::storage(&native)?;

    let listed: Vec<&str> = report
        .tables
        .iter()
        .map(|table| table.name.as_str())
        .collect();
    for name in [
        "7_1_id",
        "7_1_artifact_id",
        "7_1_content",
        "7_1_location",
        "3_1_id",
        "3_1_identity",
        "8_1_artifact_id",
        "5_1_artifact_id",
        "2_1_id",
        "2_1_key",
        "10_1_seq",
    ] {
        assert!(
            listed.contains(&name),
            "every primary and secondary table is listed, {name} is not: {listed:?}"
        );
    }
    for name in listed.iter().filter(|name| **name != "metadata") {
        let parts: Vec<&str> = name.splitn(3, '_').collect();
        assert_eq!(parts.len(), 3, "a table is named model_version_key: {name}");
        assert!(
            parts[0].parse::<u32>().is_ok() && parts[1] == "1",
            "every record is at native_model version 1: {name}"
        );
    }

    let rows = report
        .tables
        .iter()
        .find(|table| table.name.starts_with("7_1_id"))
        .ok_or_else(|| IndexError::Catalog("the entry table is reported".into()))?;
    assert_eq!(rows.entries, 2);
    assert!(rows.stored_bytes > 0, "payload bytes are reported");
    assert!(rows.leaf_pages > 0, "leaf pages are reported");
    assert_eq!(rows.tree_height, 1, "two rows fit one leaf");
    assert_eq!(
        rows.branch_pages, 0,
        "a one-level tree has no branch pages: {rows:?}"
    );

    let component_names: Vec<&str> = report
        .components
        .iter()
        .map(|component| component.name.as_str())
        .collect();
    assert!(
        component_names.contains(&"catalog.redb"),
        "redb is its own component: {component_names:?}"
    );
    assert!(
        component_names
            .iter()
            .any(|name| name.starts_with("search-v1-")),
        "the published index is its own component: {component_names:?}"
    );
    let redb = report
        .components
        .iter()
        .find(|component| component.name == "catalog.redb")
        .ok_or_else(|| IndexError::Catalog("the redb component is reported".into()))?;
    assert_eq!(redb.apparent_bytes, report.file_bytes);
    assert_eq!(redb.allocated_bytes, report.allocated_bytes);
    assert_eq!(
        report.directory_allocated_bytes,
        report
            .components
            .iter()
            .map(|component| component.allocated_bytes)
            .sum::<u64>(),
        "the directory total is the components it names"
    );
    assert!(
        report.directory_allocated_bytes > report.allocated_bytes,
        "the search index costs more than nothing beside redb"
    );

    let stored: u64 = report.tables.iter().map(|table| table.stored_bytes).sum();
    assert!(
        stored < report.file_bytes,
        "payload is what the rows spend, not what the file does: {stored} against {}",
        report.file_bytes
    );
    Ok(())
}

#[test]
fn an_entry_is_found_by_its_location_alone() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let config = IndexConfig {
        data_dir: directory.path().join("data"),
        sources: Vec::new(),
        ..IndexConfig::default()
    };
    let catalog = Catalog::open_or_scan(&config, "workstation", |_, _| {})?;
    let located = |rel_path: &str, size: u64| SeedEntry {
        node: LOCAL_NODE,
        rel_path: rel_path.into(),
        abs_path: Some(format!("/vault/{rel_path}")),
        size,
        state: State::Local,
        is_symlink: false,
        sha256: None,
        git_sha1: None,
        artifact: None,
    };
    let revision = |name: &str, entries: Vec<SeedEntry>| SeedRevision {
        name: name.into(),
        sha: None,
        refs: vec![(name.to_owned(), RefKind::Branch)],
        title: None,
        author: None,
        committed: None,
        entries,
    };
    let repo = RepoId::parse_ref(RepoType::Models, "acme/two-revisions")?;
    catalog.seed(&[SeedRepository {
        repo: repo.clone(),
        source: "cache".into(),
        kind: SourceKind::HfCache,
        root_path: "/vault".into(),
        git_dir: None,
        revisions: vec![
            revision("main", vec![located("a", 10), located("a/b", 20)]),
            revision("next", vec![located("a", 30)]),
        ],
    }])?;
    let record = repository_of(&catalog, &repo)?;

    let first = catalog
        .revision_entry(record.id, 0, "a")?
        .ok_or_else(|| IndexError::Catalog("no entry at a in revision 0".into()))?;
    assert_eq!(first.size, 10);
    let second = catalog
        .revision_entry(record.id, 1, "a")?
        .ok_or_else(|| IndexError::Catalog("no entry at a in revision 1".into()))?;
    assert_eq!(second.size, 30, "each revision holds its own path");
    assert!(catalog.revision_entry(record.id, 1, "a/b")?.is_none());

    let nested = catalog
        .revision_entry(record.id, 0, "a/b")?
        .ok_or_else(|| IndexError::Catalog("no entry at a/b".into()))?;
    assert_eq!(
        nested.size, 20,
        "a path that extends another is its own row"
    );

    let paths: Vec<String> = catalog
        .revision_entries(record.id, 0, Dedupe::Ignore)?
        .into_iter()
        .map(|entry| entry.rel_path)
        .collect();
    assert_eq!(paths, vec!["a".to_owned(), "a/b".to_owned()]);
    assert_eq!(
        catalog
            .revision_entries(record.id, 1, Dedupe::Ignore)?
            .len(),
        1
    );
    Ok(())
}

#[test]
fn location_keys_separate_repositories_revisions_and_paths() -> Result<()> {
    use native_db::ToKey;
    use native_db::db_type::Key;

    assert_eq!(
        LocationKey::repository(7).to_key(),
        Key::new(7_i64.to_be_bytes().to_vec()),
        "a repository prefix is the eight bytes of its id"
    );
    assert_eq!(
        LocationKey::revision(7, 3).to_key(),
        Key::new(
            [
                7_i64.to_be_bytes().as_slice(),
                3_u32.to_be_bytes().as_slice()
            ]
            .concat()
        ),
        "a revision prefix is twelve bytes and nothing else"
    );
    assert_eq!(
        LocationKey::at(7, 3, "a/b").to_key(),
        Key::new(
            [
                7_i64.to_be_bytes().as_slice(),
                3_u32.to_be_bytes().as_slice(),
                "a/b".as_bytes(),
            ]
            .concat()
        ),
        "the path keeps its own bytes, unfolded"
    );
    assert_eq!(
        LocationKey::at(7, 3, "").to_key(),
        LocationKey::revision(7, 3).to_key(),
        "an empty path is the revision prefix itself"
    );
    assert_ne!(
        LocationKey::at(7, 3, "a").to_key(),
        LocationKey::at(8, 3, "a").to_key(),
        "adjacent repositories do not share a key"
    );
    assert_ne!(
        LocationKey::at(7, 3, "a").to_key(),
        LocationKey::at(7, 4, "a").to_key(),
        "adjacent revisions do not share a key"
    );

    let directory = tempfile::tempdir()?;
    let config = IndexConfig {
        data_dir: directory.path().join("data"),
        sources: Vec::new(),
        ..IndexConfig::default()
    };
    let catalog = Catalog::open_or_scan(&config, "workstation", |_, _| {})?;
    let long = format!("nested/{}/weights.safetensors", "segment/".repeat(24));
    let paths = [
        "",
        "a",
        "a/b",
        "ab",
        "\u{e9}t\u{e9}/mod\u{e8}le.safetensors",
        long.as_str(),
    ];
    let located = |rel_path: &str, size: u64| SeedEntry {
        node: LOCAL_NODE,
        rel_path: rel_path.to_owned(),
        abs_path: Some(format!("/vault/{rel_path}")),
        size,
        state: State::Local,
        is_symlink: false,
        sha256: None,
        git_sha1: None,
        artifact: None,
    };
    let revision = |name: &str, offset: u64| SeedRevision {
        name: name.to_owned(),
        sha: None,
        refs: vec![(name.to_owned(), RefKind::Branch)],
        title: None,
        author: None,
        committed: None,
        entries: paths
            .iter()
            .enumerate()
            .map(|(index, path)| located(path, offset + index as u64))
            .collect(),
    };
    let repository = |name: &str| -> Result<SeedRepository> {
        Ok(SeedRepository {
            repo: RepoId::parse_ref(RepoType::Models, &format!("acme/{name}"))?,
            source: "cache".to_owned(),
            kind: SourceKind::HfCache,
            root_path: "/vault".into(),
            git_dir: None,
            revisions: vec![revision("main", 10), revision("next", 100)],
        })
    };
    let first = repository("first")?;
    let second = repository("second")?;
    catalog.seed(&[first.clone(), second.clone()])?;
    let left = repository_of(&catalog, &first.repo)?.id;
    let right = repository_of(&catalog, &second.repo)?.id;
    assert_eq!(
        right.get(),
        left.get() + 1,
        "the two repositories are adjacent ids"
    );

    for (repo, base) in [(left, 0), (right, 0)] {
        for (ordinal, offset) in [(0_u32, 10_u64), (1, 100)] {
            for (index, path) in paths.iter().enumerate() {
                let found = catalog
                    .revision_entry(repo, ordinal, path)?
                    .ok_or_else(|| IndexError::Catalog(format!("no entry at {path:?}")))?;
                assert_eq!(
                    found.size,
                    base + offset + index as u64,
                    "{path:?} of revision {ordinal} is its own row"
                );
            }
        }
    }
    assert_eq!(
        catalog.revision_entries(left, 0, Dedupe::Ignore)?.len(),
        paths.len(),
        "one revision holds exactly its own paths"
    );
    let listed: Vec<String> = catalog
        .revision_entries(left, 0, Dedupe::Ignore)?
        .into_iter()
        .map(|entry| entry.rel_path)
        .collect();
    let mut expected: Vec<String> = paths.iter().map(|path| (*path).to_owned()).collect();
    expected.sort();
    assert_eq!(listed, expected, "a revision reads back in path order");
    assert!(
        catalog.revision_entry(left, 2, "a")?.is_none(),
        "a revision the repository does not have holds nothing"
    );
    Ok(())
}

#[test]
fn computed_location_keys_follow_entry_updates() -> Result<()> {
    let (directory, catalog) = fixture()?;
    let native = directory.path().join("native");
    let read = catalog.database.r_transaction()?;
    let original: Entry = read
        .scan()
        .primary()?
        .all()?
        .next()
        .transpose()?
        .ok_or_else(|| IndexError::Catalog("the fixture has an entry".into()))?;
    drop(read);
    let repo_id = original.repo_id;
    let revision = original.revision;
    let at = |transaction: &native_db::transaction::RwTransaction<'_>,
              rel_path: &str|
     -> Result<Option<Entry>> {
        Ok(transaction.get().secondary(
            EntryKey::location,
            LocationKey::at(repo_id, revision, rel_path),
        )?)
    };

    let transaction = catalog.database.rw_transaction()?;
    let moved = Entry {
        rel_path: "moved/elsewhere.safetensors".to_owned(),
        ..original.clone()
    };
    transaction.update(original.clone(), moved.clone())?;
    assert!(
        at(&transaction, &original.rel_path)?.is_none(),
        "the key the row left is gone inside the transaction"
    );
    assert_eq!(
        at(&transaction, &moved.rel_path)?.map(|entry| entry.id),
        Some(moved.id),
        "the key the row arrived at is there"
    );
    transaction.commit()?;

    let transaction = catalog.database.rw_transaction()?;
    let duplicate = Entry {
        id: moved.id + 5000,
        ..moved.clone()
    };
    let refused = transaction.insert(duplicate);
    assert!(
        refused.is_err(),
        "a second row at one location is refused: {refused:?}"
    );
    drop(transaction);

    let transaction = catalog.database.rw_transaction()?;
    transaction.update(
        moved.clone(),
        Entry {
            rel_path: "aborted.safetensors".to_owned(),
            ..moved.clone()
        },
    )?;
    transaction.abort()?;

    drop(catalog);
    let reopened = Catalog::open(&native)?;
    let read = reopened.database.r_transaction()?;
    let after: Option<Entry> = read.get().secondary(
        EntryKey::location,
        LocationKey::at(repo_id, revision, &moved.rel_path),
    )?;
    assert_eq!(
        after.map(|entry| entry.id),
        Some(moved.id),
        "the aborted move left the committed key alone"
    );
    let aborted: Option<Entry> = read.get().secondary(
        EntryKey::location,
        LocationKey::at(repo_id, revision, "aborted.safetensors"),
    )?;
    assert!(aborted.is_none(), "an aborted move writes no key");
    let vacated: Option<Entry> = read.get().secondary(
        EntryKey::location,
        LocationKey::at(repo_id, revision, &original.rel_path),
    )?;
    assert!(vacated.is_none(), "the original key stayed removed");
    drop(read);

    let transaction = reopened.database.rw_transaction()?;
    transaction.remove(moved.clone())?;
    transaction.commit()?;
    let read = reopened.database.r_transaction()?;
    let removed: Option<Entry> = read.get().secondary(
        EntryKey::location,
        LocationKey::at(repo_id, revision, &moved.rel_path),
    )?;
    assert!(removed.is_none(), "removing the row removes its key");
    let remaining: Vec<Entry> = read
        .scan()
        .secondary::<Entry>(EntryKey::location)?
        .start_with(LocationKey::repository(repo_id))?
        .collect::<native_db::db_type::Result<_>>()?;
    assert!(
        remaining.iter().all(|entry| entry.id != moved.id),
        "the repository prefix no longer reaches it"
    );
    Ok(())
}

#[test]
fn repository_and_revision_listings_keep_their_order() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let config = IndexConfig {
        data_dir: directory.path().join("data"),
        sources: Vec::new(),
        ..IndexConfig::default()
    };
    let catalog = Catalog::open_or_scan(&config, "workstation", |_, _| {})?;
    let shuffled = [
        "m.safetensors",
        "a.safetensors",
        "z.safetensors",
        "b/c.safetensors",
    ];
    let located = |rel_path: &str, size: u64| SeedEntry {
        node: LOCAL_NODE,
        rel_path: rel_path.to_owned(),
        abs_path: Some(format!("/vault/{rel_path}")),
        size,
        state: State::Local,
        is_symlink: false,
        sha256: None,
        git_sha1: None,
        artifact: None,
    };
    let repo = RepoId::parse_ref(RepoType::Models, "acme/shuffled")?;
    catalog.seed(&[SeedRepository {
        repo: repo.clone(),
        source: "cache".to_owned(),
        kind: SourceKind::HfCache,
        root_path: "/vault".into(),
        git_dir: None,
        revisions: (0..2)
            .map(|ordinal| SeedRevision {
                name: format!("rev{ordinal}"),
                sha: None,
                refs: vec![(format!("rev{ordinal}"), RefKind::Branch)],
                title: None,
                author: None,
                committed: None,
                entries: shuffled
                    .iter()
                    .enumerate()
                    .map(|(index, path)| located(path, 100 * ordinal + index as u64 + 1))
                    .collect(),
            })
            .collect(),
    }])?;
    let record = repository_of(&catalog, &repo)?;
    assert_eq!(record.revisions.len(), 2);
    assert_eq!(
        record.present_files,
        (shuffled.len() * 2) as u32,
        "every seeded path counts once"
    );
    assert_eq!(
        record.present_bytes,
        (1..=shuffled.len() as u64).sum::<u64>() * 2 + 100 * shuffled.len() as u64,
        "the derived byte count follows the rows"
    );
    for ordinal in 0..2 {
        let listed: Vec<String> = catalog
            .revision_entries(record.id, ordinal, Dedupe::Ignore)?
            .into_iter()
            .map(|entry| entry.rel_path)
            .collect();
        assert_eq!(
            listed,
            vec![
                "a.safetensors".to_owned(),
                "b/c.safetensors".to_owned(),
                "m.safetensors".to_owned(),
                "z.safetensors".to_owned(),
            ],
            "revision {ordinal} reads back in path order whatever order it was written in"
        );
    }
    Ok(())
}

#[derive(Debug)]
/// Writes bytes into a native_db primary table that native_db itself could never put
/// there, so an old-format catalog can be built out of a current one: the rows keep the
/// native_model header naming their record and carry a body this build cannot read,
/// exactly as a genuinely older catalog's would.
struct RawKey;

impl redb::Value for RawKey {
    type SelfType<'a> = Vec<u8>;
    type AsBytes<'a>
        = Vec<u8>
    where
        Self: 'a;

    fn fixed_width() -> Option<usize> {
        None
    }

    fn from_bytes<'a>(data: &'a [u8]) -> Vec<u8>
    where
        Self: 'a,
    {
        data.to_vec()
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a Vec<u8>) -> Vec<u8>
    where
        Self: 'a,
        Self: 'b,
    {
        value.clone()
    }

    fn type_name() -> redb::TypeName {
        redb::TypeName::new("DatabaseInnerKeyValue")
    }
}

impl redb::Key for RawKey {
    fn compare(left: &[u8], right: &[u8]) -> std::cmp::Ordering {
        left.cmp(right)
    }
}

fn overwrite_rows(path: &Path, table: &str, model: u32) -> Result<usize> {
    use redb::ReadableTable;

    let value = [
        model.to_le_bytes().as_slice(),
        1_u32.to_le_bytes().as_slice(),
        b"not a body this build can read".as_slice(),
    ]
    .concat();
    let definition = redb::TableDefinition::<RawKey, &[u8]>::new(table);
    let database = redb::Database::open(path).map_err(crate::catalog::redb_error)?;
    let written;
    {
        let write = database.begin_write().map_err(crate::catalog::redb_error)?;
        {
            let mut rows = write
                .open_table(definition)
                .map_err(crate::catalog::redb_error)?;
            let keys: Vec<Vec<u8>> = rows
                .iter()
                .map_err(crate::catalog::redb_error)?
                .map(|row| row.map(|(key, _)| key.value()))
                .collect::<std::result::Result<_, _>>()
                .map_err(crate::catalog::redb_error)?;
            written = keys.len();
            for key in keys {
                rows.insert(&key, value.as_slice())
                    .map_err(crate::catalog::redb_error)?;
            }
        }
        write.commit().map_err(crate::catalog::redb_error)?;
    }
    Ok(written)
}

#[test]
fn format_version_is_ten() {
    assert_eq!(FORMAT_VERSION, 10);
}

#[test]
fn a_catalog_of_another_format_is_rebuilt_before_a_changed_record_is_decoded() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (_input, mut config) = loose_fixture(directory.path())?;
    config.data_dir = directory.path().join("data");
    let native = config.native_path();
    let catalog = Catalog::open_or_scan(&config, "workstation", |_, _| {})?;
    let built = search(&catalog, "")?.total;
    assert!(built > 0, "the fixture produced artifacts");
    drop(catalog);

    stamp_format(&native, FORMAT_VERSION - 1)?;
    let rewritten = overwrite_rows(&native.join("catalog.redb"), "7_1_id", 7)?;
    assert!(rewritten > 0, "the entry rows were made undecodable");

    let rebuilt = Catalog::open_or_scan(&config, "workstation", |_, _| {})?;
    assert_eq!(
        rebuilt.status()?.format_version,
        FORMAT_VERSION,
        "the catalog was scanned again rather than converted"
    );
    assert_eq!(
        search(&rebuilt, "")?.total,
        built,
        "the rebuilt catalog holds what the source holds"
    );
    drop(rebuilt);

    let broken = overwrite_rows(&native.join("catalog.redb"), "6_1_id", 6)?;
    assert_eq!(broken, 1, "the manifest row was made undecodable");
    let refused = Catalog::open_or_scan(&config, "workstation", |_, _| {});
    assert!(
        refused.is_err(),
        "a catalog of this format that cannot be read is an error, not permission to \
         recreate it"
    );
    assert!(
        native.join("catalog.redb").exists(),
        "nothing removed the catalog it could not read"
    );
    Ok(())
}

#[test]
fn a_catalog_whose_manifest_no_longer_decodes_and_carries_no_stamp_is_rebuilt() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (_input, mut config) = loose_fixture(directory.path())?;
    config.data_dir = directory.path().join("data");
    let native = config.native_path();
    let catalog = Catalog::open_or_scan(&config, "workstation", |_, _| {})?;
    let totals = catalog.totals()?;
    let built = search(&catalog, "")?.total;
    drop(catalog);
    fs::remove_file(native.join(FORMAT_FILE))?;
    let broken = overwrite_rows(&native.join("catalog.redb"), "6_1_id", 6)?;
    assert_eq!(broken, 1, "the manifest row was made undecodable");

    let mut phases = Vec::new();
    let rebuilt = Catalog::open_or_scan(&config, "workstation", |phase, _| {
        phases.push(phase.to_owned())
    })?;

    let status = rebuilt.status()?;
    assert_eq!(status.generation, 1, "the rebuild is a fresh scan");
    assert_eq!(status.format_version, FORMAT_VERSION);
    assert_eq!(rebuilt.totals()?, totals);
    assert_eq!(search(&rebuilt, "")?.total, built);
    assert_eq!(
        phases.first().map(String::as_str),
        Some(
            format!("rebuilding a catalog that carries no format stamp (this build writes {FORMAT_VERSION})")
                .as_str()
        ),
        "a catalog with no stamp is not reported as a format change"
    );
    Ok(())
}

#[test]
fn a_garbled_format_stamp_is_an_error_not_a_rebuild() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (_input, mut config) = loose_fixture(directory.path())?;
    config.data_dir = directory.path().join("data");
    let native = config.native_path();
    drop(Catalog::open_or_scan(&config, "workstation", |_, _| {})?);
    fs::write(native.join(FORMAT_FILE), "not a version\n")?;

    let refused = Catalog::open_or_scan(&config, "workstation", |_, _| {});

    assert!(refused.is_err(), "no build writes such a stamp");
    assert!(
        native.join("catalog.redb").exists(),
        "nothing removed the catalog behind the garbled stamp"
    );
    Ok(())
}

#[test]
fn open_or_scan_creates_then_reopens_and_tolerates_no_sources() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (_input, config) = loose_fixture(directory.path())?;
    let config = IndexConfig {
        data_dir: directory.path().join("data"),
        ..config
    };
    let first = Catalog::open_or_scan(&config, "n", |_, _| {})?;
    assert!(first.status()?.search_ready);
    assert_eq!(search(&first, "")?.total, 2);
    drop(first);
    let second = Catalog::open_or_scan(&config, "n", |_, _| {})?;
    assert_eq!(second.status()?.generation, 1, "reopening does not rescan");
    drop(second);
    let empty = IndexConfig {
        data_dir: directory.path().join("empty"),
        sources: Vec::new(),
        ..IndexConfig::default()
    };
    let none = Catalog::open_or_scan(&empty, "n", |_, _| {})?;
    assert_eq!(search(&none, "")?.total, 0);
    Ok(())
}

#[test]
fn a_catalog_of_another_format_is_rebuilt_on_open() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (_input, config) = loose_fixture(directory.path())?;
    let config = IndexConfig {
        data_dir: directory.path().join("data"),
        ..config
    };
    let expected = Catalog::scan_sources(
        &directory.path().join("reference"),
        "n",
        &config,
        HashPolicy::Never,
        |_, _| {},
    )?
    .totals()?;
    drop(Catalog::open_or_scan(&config, "n", |_, _| {})?);
    let native = config.native_path();
    stamp_format(&native, crate::catalog::FORMAT_VERSION + 1)?;

    let mut phases = Vec::new();
    let rebuilt = Catalog::open_or_scan(&config, "n", |phase, _| phases.push(phase.to_owned()))?;

    let status = rebuilt.status()?;
    assert_eq!(status.format_version, crate::catalog::FORMAT_VERSION);
    assert_eq!(status.generation, 1, "the rebuild is a fresh scan");
    assert_eq!(rebuilt.totals()?, expected);
    assert_eq!(search(&rebuilt, "")?.total, 2);
    let named = phases
        .first()
        .ok_or_else(|| IndexError::Catalog("the rebuild reported nothing".into()))?;
    assert!(
        named.contains(&format!("found {}", crate::catalog::FORMAT_VERSION + 1))
            && named.contains(&format!("writes {}", crate::catalog::FORMAT_VERSION)),
        "the first thing a rebuild reports names both formats: {named:?}"
    );
    Ok(())
}

#[test]
fn a_rebuild_names_why_it_is_rebuilding() {
    let directory = PathBuf::from("/tmp/native");
    let changed = crate::catalog::rebuild_reason(&IndexError::CatalogFormat {
        directory: directory.clone(),
        found: Some(9),
        expected: 10,
    });
    assert_eq!(
        changed,
        "rebuilding after a catalog format change (found 9, this build writes 10)"
    );

    let unstamped = crate::catalog::rebuild_reason(&IndexError::CatalogFormat {
        directory: directory.clone(),
        found: None,
        expected: 10,
    });
    assert_eq!(
        unstamped,
        "rebuilding a catalog that carries no format stamp (this build writes 10)"
    );

    let unfinished = crate::catalog::rebuild_reason(&IndexError::CatalogIncomplete { directory });
    assert_eq!(unfinished, "rebuilding a catalog whose scan never finished");
}

#[test]
fn open_refuses_another_format() -> Result<()> {
    let (directory, catalog) = fixture()?;
    let native = directory.path().join("native");
    drop(catalog);
    stamp_format(&native, crate::catalog::FORMAT_VERSION + 1)?;
    match Catalog::open(&native) {
        Err(IndexError::CatalogFormat {
            directory: named,
            found,
            expected,
        }) => {
            assert_eq!(named, native);
            assert_eq!(found, Some(crate::catalog::FORMAT_VERSION + 1));
            assert_eq!(expected, crate::catalog::FORMAT_VERSION);
        }
        other => panic!("a catalog of another format is refused: {other:?}"),
    }
    Ok(())
}

fn stamp_format(directory: &Path, format_version: u32) -> Result<()> {
    fs::write(directory.join(FORMAT_FILE), format!("{format_version}\n"))?;
    Ok(())
}
