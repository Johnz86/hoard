use std::fs;
use std::path::Path;

use tempfile::TempDir;

use crate::config::{IndexConfig, SourceConfig, SourceKind};

use crate::error::{IndexError, Result};

use super::models::{ArtifactRecord, ArtifactRecordKey, Entry, Manifest, SearchEntry, Source};
use super::{Catalog, SearchRequest, models};

fn running_as_root() -> bool {
    use std::os::unix::fs::MetadataExt;
    fs::metadata("/proc/self").is_ok_and(|metadata| metadata.uid() == 0)
}

fn source(path: &Path, name: &str) -> SourceConfig {
    SourceConfig {
        name: name.to_owned(),
        path: path.to_path_buf(),
        kind: SourceKind::Loose,
        include: Vec::new(),
        exclude: Vec::new(),
        follow_symlinks: false,
        repo_per_folder: false,
    }
}

fn fixture() -> Result<(TempDir, Catalog)> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir(&input)?;
    fs::write(input.join("training.csv"), "prompt,response\nhello,world\n")?;
    fs::write(input.join("tiny.jsonl"), "{\"text\":\"hello\"}\n")?;
    let config = IndexConfig {
        sources: vec![source(&input, "datasets")],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "test-node",
        &config,
        |_, _| {},
    )?;
    Ok((directory, catalog))
}

fn search(catalog: &Catalog, query: &str) -> Result<super::SearchPage> {
    catalog.search(SearchRequest {
        query,
        limit: 50,
        offset: 0,
    })
}

#[test]
fn sources_scan_into_typed_native_records_and_reopen_without_sqlite() -> Result<()> {
    let (directory, catalog) = fixture()?;
    let status = catalog.status()?;
    assert_eq!(status.node, "test-node");
    assert!(status.counts.contains(&("entries".into(), 2)));
    assert!(!status.search_ready);
    assert!(search(&catalog, "").is_err());
    {
        let read = catalog.database.r_transaction()?;
        let sources: Vec<Source> = read
            .scan()
            .primary()?
            .all()?
            .collect::<native_db::db_type::Result<_>>()?;
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].resolved_kind, SourceKind::Loose);
        assert_eq!(read.len().primary::<ArtifactRecord>()?, 2);
        assert_eq!(read.len().primary::<SearchEntry>()?, 2);
    }
    catalog.rebuild_search(|_| {})?;
    let found = search(&catalog, "format:csv")?;
    assert_eq!(found.total, 1);
    assert_eq!(found.rows[0].name, "training.csv");
    assert_eq!(found.rows[0].node, "test-node");
    drop(catalog);
    let mut reopened = Catalog::open(&directory.path().join("native"))?;
    assert!(reopened.check_integrity()?);
    assert!(reopened.status()?.search_ready);
    assert_eq!(search(&reopened, "")?.total, 2);
    assert!(!directory.path().join("native/hoard.db").exists());
    Ok(())
}

#[test]
fn rebuilding_search_needs_only_native_records() -> Result<()> {
    let (directory, catalog) = fixture()?;
    catalog.rebuild_search(|_| {})?;
    let original = search(&catalog, "")?;
    fs::remove_dir_all(directory.path().join("files"))?;
    let old_directory = catalog.manifest()?.search_directory;
    catalog.rebuild_search(|_| {})?;
    assert_ne!(catalog.manifest()?.search_directory, old_directory);
    assert_eq!(search(&catalog, "")?, original);
    Ok(())
}

#[test]
fn filtering_ranges_boolean_queries_and_pagination_are_deterministic() -> Result<()> {
    let (_directory, catalog) = fixture()?;
    catalog.rebuild_search(|_| {})?;
    let all = search(&catalog, "")?;
    assert!(all.rows[0].size > all.rows[1].size);
    assert_eq!(search(&catalog, "format:csv AND node:test-node")?.total, 1);
    assert_eq!(search(&catalog, "format:csv OR format:jsonl")?.total, 2);
    assert_eq!(
        search(&catalog, "size:[0 TO 20]")?.rows[0].name,
        "tiny.jsonl"
    );
    assert_eq!(search(&catalog, "training")?.total, 1);
    let first = catalog.search(SearchRequest {
        query: "",
        limit: 1,
        offset: 0,
    })?;
    let second = catalog.search(SearchRequest {
        query: "",
        limit: 1,
        offset: 1,
    })?;
    assert_eq!(first.total, 2);
    assert_eq!(first.rows, vec![all.rows[0].clone()]);
    assert_eq!(second.rows, vec![all.rows[1].clone()]);
    assert!(
        catalog
            .search(SearchRequest {
                query: "",
                limit: 0,
                offset: 0
            })?
            .rows
            .is_empty()
    );
    assert!(
        catalog
            .search(SearchRequest {
                query: "",
                limit: 50,
                offset: u32::MAX
            })?
            .rows
            .is_empty()
    );
    assert!(
        catalog
            .search(SearchRequest {
                query: "",
                limit: 1001,
                offset: 0
            })
            .is_err()
    );
    assert!(search(&catalog, "unknown_field:csv").is_err());
    Ok(())
}

#[test]
fn duplicate_paths_collapse_after_correlated_entry_filtering() -> Result<()> {
    let (_directory, catalog) = fixture()?;
    let transaction = catalog.database.rw_transaction()?;
    let mut entry = transaction
        .scan()
        .primary::<SearchEntry>()?
        .all()?
        .next()
        .ok_or_else(|| IndexError::Catalog("missing fixture entry".into()))??;
    let original_node = entry.row.node.clone();
    let artifact_id = entry.artifact_id;
    let mut artifact: ArtifactRecord = transaction
        .get()
        .primary(artifact_id)?
        .ok_or_else(|| IndexError::Catalog("missing fixture artifact".into()))?;
    artifact.path_count = 2;
    transaction.upsert(artifact)?;
    entry.row.path_count = 2;
    entry.row.dupe = true;
    transaction.upsert(entry.clone())?;
    entry.entry_id = 900;
    entry.row.entry_id = hoard_core::EntryId::new(900);
    entry.row.node = "peer".into();
    entry.row.state = hoard_core::State::Remote;
    entry.preference = 1;
    transaction.insert(entry)?;
    transaction.commit()?;
    catalog.rebuild_search(|_| {})?;
    let all = search(&catalog, "")?;
    assert_eq!(all.total, 2);
    let shared = all
        .rows
        .iter()
        .find(|row| row.artifact_id.get() == artifact_id)
        .ok_or_else(|| IndexError::Catalog("missing shared result".into()))?;
    assert_eq!(shared.node, original_node);
    assert!(shared.dupe);
    assert_eq!(shared.path_count, 2);
    assert_eq!(search(&catalog, "node:peer")?.rows[0].node, "peer");
    assert_eq!(search(&catalog, "node:peer AND state:local")?.total, 0);
    Ok(())
}

#[test]
fn native_secondary_indexes_update_and_abort_atomically() -> Result<()> {
    let (_directory, catalog) = fixture()?;
    let transaction = catalog.database.rw_transaction()?;
    let mut artifact = transaction
        .scan()
        .primary::<ArtifactRecord>()?
        .all()?
        .next()
        .ok_or_else(|| IndexError::Catalog("missing fixture artifact".into()))??;
    let original = artifact.identity.clone();
    artifact.identity = "replacement".into();
    transaction.upsert(artifact.clone())?;
    let missing: Option<ArtifactRecord> = transaction
        .get()
        .secondary(ArtifactRecordKey::identity, original.as_str())?;
    assert!(missing.is_none());
    transaction.abort()?;
    let read = catalog.database.r_transaction()?;
    let restored: Option<ArtifactRecord> = read
        .get()
        .secondary(ArtifactRecordKey::identity, original.as_str())?;
    assert!(restored.is_some());
    let aborted: Option<ArtifactRecord> = read
        .get()
        .secondary(ArtifactRecordKey::identity, "replacement")?;
    assert!(aborted.is_none());
    Ok(())
}

#[test]
fn incomplete_or_unknown_catalogs_are_refused() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database =
        native_db::Builder::new().create(models()?, directory.path().join("catalog.redb"))?;
    drop(database);
    assert!(Catalog::open(directory.path()).is_err());
    let (_directory, catalog) = fixture()?;
    let mut manifest = catalog.manifest()?;
    manifest.format_version = 999;
    let transaction = catalog.database.rw_transaction()?;
    transaction.upsert(manifest)?;
    transaction.commit()?;
    let path = catalog.directory.clone();
    drop(catalog);
    assert!(Catalog::open(&path).is_err());
    Ok(())
}

#[test]
fn invalid_output_and_sources_do_not_overwrite_existing_data() -> Result<()> {
    let (directory, catalog) = fixture()?;
    let config = IndexConfig {
        sources: vec![source(&directory.path().join("files"), "datasets")],
        ..IndexConfig::default()
    };
    assert!(Catalog::scan_sources(&catalog.directory, "node", &config, |_, _| {}).is_err());
    assert!(
        Catalog::scan_sources(
            &directory.path().join("files/nested"),
            "node",
            &config,
            |_, _| {}
        )
        .is_err()
    );
    assert!(!directory.path().join("files/nested").exists());
    assert_eq!(
        catalog.status()?.counts.last(),
        Some(&("entries".into(), 2))
    );
    assert!(
        Catalog::scan_sources(
            &directory.path().join("empty"),
            "node",
            &IndexConfig::default(),
            |_, _| {}
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn unpublished_rebuilds_do_not_change_the_previous_search_manifest() -> Result<()> {
    let (_directory, catalog) = fixture()?;
    catalog.rebuild_search(|_| {})?;
    let original = catalog.manifest()?;
    fs::create_dir(catalog.directory.join("search-v1-interrupted"))?;
    let reopened_manifest: Manifest = catalog
        .database
        .r_transaction()?
        .get()
        .primary(0_u8)?
        .ok_or_else(|| IndexError::Catalog("missing fixture manifest".into()))?;
    assert_eq!(reopened_manifest, original);
    assert_eq!(search(&catalog, "")?.total, 2);
    Ok(())
}

#[test]
fn a_catalog_open_elsewhere_reports_the_lock_plainly() -> Result<()> {
    let (directory, catalog) = fixture()?;
    let second = Catalog::open(&directory.path().join("native"));
    let message = match second {
        Err(IndexError::Catalog(message)) => message,
        other => panic!("expected a catalog error, got {other:?}"),
    };
    assert!(message.contains("another process"), "{message}");
    drop(catalog);
    Ok(())
}

#[test]
fn a_missing_source_root_is_skipped_not_fatal() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir(&input)?;
    fs::write(input.join("tiny.jsonl"), "{\"text\":\"hello\"}\n")?;
    let config = IndexConfig {
        sources: vec![
            source(&input, "present"),
            source(&directory.path().join("unmounted"), "absent"),
        ],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "test-node",
        &config,
        |_, _| {},
    )?;
    let status = catalog.status()?;
    assert!(status.counts.contains(&("sources".into(), 1)));
    assert!(status.counts.contains(&("entries".into(), 1)));
    Ok(())
}

#[test]
fn a_failed_scan_leaves_no_directory_behind() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let not_a_directory = directory.path().join("file-as-root");
    fs::write(&not_a_directory, "x")?;
    let config = IndexConfig {
        sources: vec![source(&not_a_directory, "broken")],
        ..IndexConfig::default()
    };
    let output = directory.path().join("native");
    let result = Catalog::scan_sources(&output, "test-node", &config, |_, _| {});
    assert!(result.is_err());
    assert!(!output.exists());
    Ok(())
}

#[test]
fn an_unreadable_file_is_recorded_as_absent_instead_of_aborting() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if running_as_root() {
        return Ok(());
    }
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir(&input)?;
    fs::write(input.join("readable.csv"), "a,b\n1,2\n")?;
    let locked = input.join("locked.csv");
    fs::write(&locked, "a,b\n1,2\n")?;
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000))?;
    let config = IndexConfig {
        sources: vec![source(&input, "files")],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "test-node",
        &config,
        |_, _| {},
    )?;
    catalog.rebuild_search(|_| {})?;
    let rows = search(&catalog, "")?;
    assert_eq!(rows.total, 1, "the unreadable file proves no artifact");
    assert_eq!(rows.rows[0].name, "readable.csv");
    let read = catalog.database.r_transaction()?;
    let entries: Vec<Entry> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    let absent = entries
        .iter()
        .find(|entry| entry.rel_path == "locked.csv")
        .ok_or_else(|| IndexError::Catalog("missing unreadable entry".into()))?;
    assert_eq!(entries.len(), 2, "the unreadable file is still an entry");
    assert_eq!(absent.state, hoard_core::State::Remote);
    assert!(
        absent.abs_path.is_some(),
        "the file is on this disk even though it could not be read"
    );
    assert!(absent.artifact_id.is_none());
    drop(read);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[test]
fn an_lfs_pointer_is_an_absent_entry_without_an_artifact() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir(&input)?;
    let oid = "a".repeat(64);
    fs::write(
        input.join("model.safetensors"),
        format!("version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize 123456\n"),
    )?;
    let config = IndexConfig {
        sources: vec![source(&input, "files")],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(
        &directory.path().join("native"),
        "test-node",
        &config,
        |_, _| {},
    )?;
    catalog.rebuild_search(|_| {})?;
    assert_eq!(
        search(&catalog, "")?.total,
        0,
        "a pointer is not a model that can be served"
    );
    let read = catalog.database.r_transaction()?;
    let entries: Vec<Entry> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert!(entry.lfs_pointer);
    assert_eq!(entry.size, 123_456);
    assert_eq!(entry.state, hoard_core::State::Remote);
    assert!(entry.abs_path.is_none());
    assert!(
        entry.artifact_id.is_none(),
        "the SQLite scanner writes no blob and no artifact for a pointer"
    );
    assert_eq!(read.len().primary::<ArtifactRecord>()?, 0);
    Ok(())
}

fn discovered(
    rel_path: &str,
    abs_path: Option<&Path>,
    lfs_pointer: bool,
) -> crate::discover::DiscoveredEntry {
    crate::discover::DiscoveredEntry {
        rel_path: rel_path.to_owned(),
        abs_path: abs_path.map(Path::to_path_buf),
        size: 0,
        stamp: None,
        is_symlink: false,
        sha256: None,
        git_sha1: None,
        lfs_pointer,
    }
}

#[test]
fn a_fetched_lfs_weight_is_an_artifact_and_an_unfetched_pointer_is_not() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let weights = directory.path().join("model.safetensors");
    fs::write(&weights, lora_header())?;
    let fetched = discovered("model.safetensors", Some(&weights), true);
    let sniffed = crate::sniff::sniff_path(&weights)?;
    let presence = super::scan::Presence::of(&fetched, Some(&sniffed));
    assert!(presence.readable);
    assert!(
        presence.lfs_pointer,
        "discovery's flag is kept for the entry row"
    );
    assert!(
        !presence.pointer_bytes,
        "the bytes on disk are weights, not a pointer"
    );
    assert!(super::scan::creates_artifact(
        sniffed.format,
        &presence,
        &super::scan::EntryRole::Standalone
    ));

    let pointer = directory.path().join("pointer.safetensors");
    let oid = "a".repeat(64);
    fs::write(
        &pointer,
        format!("version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize 5\n"),
    )?;
    let unfetched = discovered("pointer.safetensors", Some(&pointer), false);
    let sniffed = crate::sniff::sniff_path(&pointer)?;
    let presence = super::scan::Presence::of(&unfetched, Some(&sniffed));
    assert!(presence.pointer_bytes);
    assert!(!presence.readable);
    assert!(!super::scan::creates_artifact(
        sniffed.format,
        &presence,
        &super::scan::EntryRole::Standalone
    ));
    Ok(())
}

fn plain_header() -> Vec<u8> {
    crate::sniff::tests::safetensors_fixture(concat!(
        r#"{"model.layers.0.mlp.down_proj.weight":"#,
        r#"{"dtype":"F16","shape":[16,64],"data_offsets":[0,2048]}}"#
    ))
}

fn lora_header() -> Vec<u8> {
    crate::sniff::tests::safetensors_fixture(concat!(
        r#"{"__metadata__":{"ss_network_dim":"16","ss_network_alpha":"8"},"#,
        r#""lora_unet_down_blocks_0_attn.lora_down.weight":"#,
        r#"{"dtype":"F16","shape":[16,768],"data_offsets":[0,24576]},"#,
        r#""lora_unet_down_blocks_0_attn.lora_up.weight":"#,
        r#"{"dtype":"F16","shape":[768,16],"data_offsets":[24576,49152]}}"#
    ))
}

fn hf_cache_repo(root: &Path, org: &str, name: &str, files: &[(&str, Vec<u8>)]) -> Result<()> {
    let repo = root.join(format!("models--{org}--{name}"));
    let commit = "b".repeat(40);
    let snapshot = repo.join("snapshots").join(&commit);
    fs::create_dir_all(&snapshot)?;
    fs::create_dir_all(repo.join("blobs"))?;
    fs::create_dir_all(repo.join("refs"))?;
    fs::write(repo.join("refs/main"), &commit)?;
    for (index, (rel_path, content)) in files.iter().enumerate() {
        let blob = repo.join("blobs").join(format!("{index:064x}"));
        fs::write(&blob, content)?;
        let link = snapshot.join(rel_path);
        if let Some(parent) = link.parent() {
            fs::create_dir_all(parent)?;
        }
        std::os::unix::fs::symlink(&blob, &link)?;
    }
    Ok(())
}

#[test]
fn hf_cache_rows_match_the_legacy_shape() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    hf_cache_repo(
        &hub,
        "acme",
        "styler",
        &[
            (
                "README.md",
                b"---\ninstance_prompt: acme style\n---\nA style LoRA.\n".to_vec(),
            ),
            (
                "config.json",
                b"{\"architectures\":[\"UNet2DConditionModel\"]}".to_vec(),
            ),
            ("unet/adapter.safetensors", lora_header()),
        ],
    )?;
    let config = IndexConfig {
        sources: vec![SourceConfig {
            kind: SourceKind::HfCache,
            ..source(&hub, "hub")
        }],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(&directory.path().join("native"), "n", &config, |_, _| {})?;
    catalog.rebuild_search(|_| {})?;
    let rows = search(&catalog, "")?;
    assert_eq!(
        rows.total, 1,
        "only the weights become an artifact, not README or config"
    );
    let row = &rows.rows[0];
    assert_eq!(row.org.as_deref(), Some("acme"));
    assert_eq!(
        row.name, "styler",
        "a single-artifact repo is named by its repo"
    );
    let read = catalog.database.r_transaction()?;
    assert_eq!(
        read.len().primary::<Entry>()?,
        3,
        "every file is still an entry"
    );
    Ok(())
}

#[test]
fn a_revisions_own_readme_and_config_reach_inference() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir_all(input.join("unet"))?;
    fs::write(
        input.join("README.md"),
        "---\ninstance_prompt: acme style\n---\nA style LoRA.\n",
    )?;
    fs::write(
        input.join("config.json"),
        "{\"architectures\":[\"LlamaForCausalLM\"]}",
    )?;
    fs::write(input.join("unet/adapter.safetensors"), plain_header())?;
    let config = IndexConfig {
        sources: vec![source(&input, "files")],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(&directory.path().join("native"), "n", &config, |_, _| {})?;
    catalog.rebuild_search(|_| {})?;
    let rows = search(&catalog, "")?;
    assert_eq!(
        rows.total, 1,
        "only the weights become an artifact, not README or config"
    );
    let row = &rows.rows[0];
    assert_eq!(
        row.name, "adapter.safetensors",
        "a loose file is named by its own file name"
    );
    assert_eq!(
        row.trigger.as_deref(),
        Some("acme style"),
        "the README frontmatter reached inference"
    );
    assert_ne!(
        row.trigger.as_deref(),
        Some("unet/adapter"),
        "the rel path never leaks into the trigger"
    );
    assert_eq!(
        search(&catalog, "\"style LoRA\"")?.total,
        1,
        "the README body is searchable text"
    );
    let read = catalog.database.r_transaction()?;
    assert_eq!(
        read.len().primary::<Entry>()?,
        3,
        "every file is still an entry"
    );
    let records: Vec<ArtifactRecord> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    assert_eq!(records.len(), 1);
    assert!(
        records[0].evidence.iter().any(|evidence| {
            evidence.field == "role" && evidence.source == hoard_core::EvidenceSource::Config
        }),
        "the neighbouring config.json was consulted: {:?}",
        records[0].evidence
    );
    Ok(())
}

#[test]
fn path_counts_are_final_in_the_stored_row() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("files");
    fs::create_dir_all(input.join("a"))?;
    fs::create_dir_all(input.join("b"))?;
    fs::write(input.join("a/same.csv"), "x,y\n1,2\n")?;
    std::os::unix::fs::symlink(input.join("a/same.csv"), input.join("b/same.csv"))?;
    let config = IndexConfig {
        sources: vec![SourceConfig {
            follow_symlinks: true,
            ..source(&input, "files")
        }],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(&directory.path().join("native"), "n", &config, |_, _| {})?;
    let read = catalog.database.r_transaction()?;
    let rows: Vec<SearchEntry> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .all(|entry| entry.row.path_count == 2 && entry.row.dupe)
    );
    let first: SearchEntry = read
        .get()
        .primary(1_i64)?
        .ok_or_else(|| IndexError::Catalog("first row missing".into()))?;
    assert_eq!(
        first.row.path_count, 2,
        "only the final sweep can have set the first row"
    );
    let record: ArtifactRecord = read
        .get()
        .primary(first.artifact_id)?
        .ok_or_else(|| IndexError::Catalog("artifact missing".into()))?;
    assert_eq!(record.path_count, 2);
    Ok(())
}

#[test]
fn search_entry_encoding_is_frozen_until_format_version_is_bumped() -> Result<()> {
    let (_directory, catalog) = fixture()?;
    let read = catalog.database.r_transaction()?;
    let mut rows: Vec<SearchEntry> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    rows.sort_by_key(|entry| entry.entry_id);
    let mut first = rows
        .into_iter()
        .next()
        .ok_or_else(|| IndexError::Catalog("no row".into()))?;
    first.row.abs_path = None;
    first.row.path = "fixed".into();
    let encoded =
        native_model::encode(&first).map_err(|error| IndexError::Catalog(error.to_string()))?;
    insta::assert_snapshot!(hex::encode(encoded));
    Ok(())
}

fn shard_header(keys: &[&str], width: u64) -> Vec<u8> {
    let bytes = width * width * 2;
    let entries: Vec<String> = keys
        .iter()
        .enumerate()
        .map(|(index, key)| {
            let start = index as u64 * bytes;
            format!(
                r#""{key}":{{"dtype":"F16","shape":[{width},{width}],"data_offsets":[{start},{}]}}"#,
                start + bytes
            )
        })
        .collect();
    crate::sniff::tests::safetensors_fixture(&format!("{{{}}}", entries.join(",")))
}

fn llm_shards() -> Vec<Vec<u8>> {
    vec![
        shard_header(&["model.layers.0.self_attn.q_proj.weight"], 64),
        shard_header(&["model.layers.1.self_attn.q_proj.weight"], 48),
        shard_header(&["lm_head.weight"], 32),
    ]
}

fn scan_hub(directory: &Path, hub: &Path) -> Result<Catalog> {
    let config = IndexConfig {
        sources: vec![SourceConfig {
            kind: SourceKind::HfCache,
            ..source(hub, "hub")
        }],
        ..IndexConfig::default()
    };
    let catalog = Catalog::scan_sources(&directory.join("native"), "n", &config, |_, _| {})?;
    catalog.rebuild_search(|_| {})?;
    Ok(catalog)
}

#[test]
fn a_sharded_model_is_one_native_artifact_over_every_file() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    let shards = llm_shards();
    let total: u64 = shards.iter().map(|bytes| bytes.len() as u64).sum();
    hf_cache_repo(
        &hub,
        "Qwen",
        "Qwen3-Sharded",
        &[
            ("model-00001-of-00003.safetensors", shards[0].clone()),
            ("model-00002-of-00003.safetensors", shards[1].clone()),
            ("model-00003-of-00003.safetensors", shards[2].clone()),
        ],
    )?;
    let catalog = scan_hub(directory.path(), &hub)?;
    let rows = search(&catalog, "")?;
    assert_eq!(rows.total, 1, "the shards are one model");
    let row = &rows.rows[0];
    assert_eq!(row.role, hoard_core::Role::Llm);
    assert_eq!(row.name, "Qwen3-Sharded");
    assert_eq!(row.org.as_deref(), Some("Qwen"));
    assert_eq!(row.size, total, "the row weighs the whole model");
    assert_eq!(row.shard_count, 3);
    assert!(!row.triage);
    assert_eq!(
        row.path, "model-00001-of-00003.safetensors",
        "the anchor is the lowest shard"
    );
    let read = catalog.database.r_transaction()?;
    let entries: Vec<Entry> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    assert_eq!(entries.len(), 3, "every shard is still an entry");
    assert!(
        entries
            .iter()
            .all(|entry| entry.artifact_id == Some(row.artifact_id.get()))
    );
    assert_eq!(read.len().primary::<ArtifactRecord>()?, 1);
    assert_eq!(
        read.len().primary::<SearchEntry>()?,
        1,
        "members get no search row of their own"
    );
    assert!(catalog.status()?.counts.contains(&("artifacts".into(), 1)));
    Ok(())
}

#[test]
fn an_incomplete_shard_set_is_triage_and_counts_what_it_has() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    let shards = llm_shards();
    hf_cache_repo(
        &hub,
        "Qwen",
        "Qwen3-Partial",
        &[
            ("model-00001-of-00003.safetensors", shards[0].clone()),
            ("model-00002-of-00003.safetensors", shards[1].clone()),
        ],
    )?;
    let catalog = scan_hub(directory.path(), &hub)?;
    let rows = search(&catalog, "state:triage")?;
    assert_eq!(rows.total, 1);
    assert_eq!(rows.rows[0].shard_count, 2);
    assert!(rows.rows[0].triage);
    Ok(())
}

#[test]
fn a_weight_map_groups_shards_the_naming_convention_would_miss() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    let shards = llm_shards();
    let index = br#"{"metadata":{},"weight_map":{"model.layers.0.self_attn.q_proj.weight":"part-a.safetensors","model.layers.1.self_attn.q_proj.weight":"part-b.safetensors","lm_head.weight":"part-c.safetensors"}}"#.to_vec();
    hf_cache_repo(
        &hub,
        "Qwen",
        "Qwen3-Mapped",
        &[
            ("model.safetensors.index.json", index),
            ("part-a.safetensors", shards[0].clone()),
            ("part-b.safetensors", shards[1].clone()),
            ("part-c.safetensors", shards[2].clone()),
        ],
    )?;
    let catalog = scan_hub(directory.path(), &hub)?;
    let rows = search(&catalog, "")?;
    assert_eq!(rows.total, 1, "the index document names the pieces");
    assert_eq!(rows.rows[0].shard_count, 3);
    assert_eq!(rows.rows[0].name, "Qwen3-Mapped");
    Ok(())
}

#[test]
fn two_shard_sets_in_one_directory_are_told_apart_by_stem() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let hub = directory.path().join("hub");
    fs::create_dir(&hub)?;
    let shards = llm_shards();
    hf_cache_repo(
        &hub,
        "Qwen",
        "Qwen3-Twins",
        &[
            ("model-00001-of-00002.safetensors", shards[0].clone()),
            ("model-00002-of-00002.safetensors", shards[2].clone()),
            ("draft-00001-of-00002.safetensors", shards[1].clone()),
            ("draft-00002-of-00002.safetensors", shards[2].clone()),
        ],
    )?;
    let catalog = scan_hub(directory.path(), &hub)?;
    let rows = search(&catalog, "")?;
    assert_eq!(rows.total, 2);
    let mut names: Vec<&str> = rows.rows.iter().map(|row| row.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        ["draft", "model"],
        "two sets at the root are named by stem"
    );
    Ok(())
}
