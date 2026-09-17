use super::*;

#[test]
fn compact_records_round_trip_without_changing_public_views() -> Result<()> {
    use strum::IntoEnumIterator;

    let directory = tempfile::tempdir()?;
    let config = IndexConfig {
        data_dir: directory.path().join("data"),
        sources: Vec::new(),
        ..IndexConfig::default()
    };
    let native = config.native_path();
    let catalog = Catalog::open_or_scan(&config, "workstation", |_, _| {})?;
    let sha = Sha256::from_bytes([0x5a; 32]);
    let git = CommitSha::from_bytes([0x7b; 20]);
    let commit = CommitSha::from_bytes([0x11; 20]);
    let tensors = TensorSummary {
        tensor_count: 4_294_967_295,
        params: u64::MAX,
        dtypes: BTreeMap::from([("F16".to_owned(), 12), ("BF16".to_owned(), 1)]),
        key_prefixes: vec![("lora_unet".to_owned(), 8), ("text_model".to_owned(), 2)],
        samples: vec![TensorSample {
            name: "lora_unet.lora_down.weight".to_owned(),
            dtype: "F16".to_owned(),
            shape: vec![32, 640, u64::MAX],
        }],
        metadata: BTreeMap::from([("format".to_owned(), "pt".to_owned())]),
        ..Default::default()
    };
    let evidence: Vec<Evidence> = [
        EvidenceSource::Tensor,
        EvidenceSource::Config,
        EvidenceSource::Metadata,
        EvidenceSource::Frontmatter,
        EvidenceSource::FolderHint,
        EvidenceSource::Filename,
        EvidenceSource::Header,
        EvidenceSource::Lookup,
        EvidenceSource::User,
    ]
    .into_iter()
    .map(|source| Evidence {
        field: "role".to_owned(),
        value: source.as_str().to_owned(),
        source,
        detail: match source {
            EvidenceSource::User => None,
            EvidenceSource::Tensor
            | EvidenceSource::Config
            | EvidenceSource::Metadata
            | EvidenceSource::Frontmatter
            | EvidenceSource::FolderHint
            | EvidenceSource::Filename
            | EvidenceSource::Header
            | EvidenceSource::Lookup => Some("read from the header".to_owned()),
        },
        confidence: 0.5,
    })
    .collect();
    let mut seeded = Vec::new();
    for (index, (role, format)) in Role::iter().zip(ArtifactFormat::iter().cycle()).enumerate() {
        for (offset, state) in State::iter().enumerate() {
            let ordinal = (index * 16 + offset) as u64;
            seeded.push((role, format, state, ordinal));
        }
    }
    let sub_roles: Vec<Option<SubRole>> = std::iter::once(None)
        .chain(SubRole::iter().map(Some))
        .collect();
    let entries: Vec<SeedEntry> = seeded
        .iter()
        .enumerate()
        .map(|(index, (role, format, state, ordinal))| SeedEntry {
            node: LOCAL_NODE,
            rel_path: format!("w{index:04}.safetensors"),
            abs_path: match state {
                State::Remote => None,
                _ => Some(format!("/vault/w{index:04}.safetensors")),
            },
            size: u64::MAX - ordinal,
            state: *state,
            is_symlink: index.is_multiple_of(3),
            sha256: Some(sha),
            git_sha1: Some(git),
            artifact: Some(SeedArtifact {
                identity: u128::MAX - u128::from(*ordinal),
                artifact: Artifact {
                    id: ArtifactId::new(0),
                    blob: BlobId::new(0),
                    sha256: Some(sha),
                    size: u64::MAX - ordinal,
                    format: *format,
                    role: *role,
                    sub_role: sub_roles[index % sub_roles.len()],
                    role_confidence: 0.875,
                    base: Base::new("sdxl 1.0"),
                    base_label: Some("SDXL 1.0".to_owned()),
                    base_confidence: 0.25,
                    precision: Some(Precision::from_dtype("F16")),
                    quant: Some("Q4_K_M".to_owned()),
                    rank: Some(u32::MAX),
                    alpha: Some(-1.5),
                    params: Some(u64::MAX),
                    context: Some(u32::MAX),
                    trigger: Some("ohwx style".to_owned()),
                },
                name: format!("w{index:04}.safetensors"),
                org: Some("acme".to_owned()),
                tensors: Some(tensors.clone()),
                evidence: evidence.clone(),
                card: Some("# card\n\nbody".to_owned()),
                group_bytes: Some(u64::MAX),
                shard_count: Some(u32::MAX),
            }),
        })
        .collect();
    let repo = RepoId::parse_ref(RepoType::Models, "acme/every-shape")?;
    catalog.seed(&[SeedRepository {
        repo: repo.clone(),
        source: "cache".to_owned(),
        kind: SourceKind::HfCache,
        root_path: "/vault".into(),
        git_dir: None,
        tool_layout: None,
        revisions: vec![SeedRevision {
            name: "main".to_owned(),
            sha: Some(commit),
            refs: vec![("main".to_owned(), RefKind::Branch)],
            title: Some("a commit".to_owned()),
            author: Some("someone".to_owned()),
            committed: jiff::Timestamp::from_second(1_700_000_000).ok(),
            entries,
        }],
    }])?;
    catalog.rebuild_search(|_| {})?;
    let before = search(&catalog, "")?;
    let read = catalog.database.r_transaction()?;
    let written: Vec<Entry> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    drop(read);
    assert_eq!(written.len(), seeded.len(), "every seed became a row");
    let states: BTreeMap<State, usize> =
        written.iter().fold(BTreeMap::new(), |mut counted, entry| {
            *counted.entry(entry.state).or_default() += 1;
            counted
        });
    assert_eq!(
        states.len(),
        State::iter().count(),
        "every state variant is on a row: {states:?}"
    );
    let present = seeded
        .iter()
        .filter(|(_, _, state, _)| *state != State::Remote)
        .count();
    assert_eq!(
        before.total as usize, present,
        "the sweep drops an artifact no present path reaches"
    );
    drop(catalog);

    let reopened = Catalog::open(&native)?;
    let after = search(&reopened, "")?;
    assert_eq!(
        after.rows, before.rows,
        "every row reads back the way it was written"
    );
    let read = reopened.database.r_transaction()?;
    let reread: Vec<Entry> = read
        .scan()
        .primary()?
        .all()?
        .collect::<native_db::db_type::Result<_>>()?;
    drop(read);
    assert_eq!(
        reread, written,
        "every field of every entry survives the compact form"
    );
    let row = after
        .rows
        .first()
        .ok_or_else(|| IndexError::Catalog("a row".into()))?;
    assert_eq!(row.blob_sha256, Some(sha), "a hash survives byte for byte");
    assert_eq!(row.revision_sha, Some(commit));
    assert_eq!(
        row.params,
        Some(u64::MAX),
        "a large integer is not truncated"
    );
    assert_eq!(row.rank, Some(u32::MAX));
    assert_eq!(row.alpha, Some(-1.5), "a negative float keeps its sign");
    assert_eq!(row.size, u64::MAX, "the largest size survives");

    let detail = reopened
        .artifact_detail(row.artifact_id)?
        .ok_or_else(|| IndexError::Catalog("a detail".into()))?;
    assert_eq!(detail.tensors.as_ref(), Some(&tensors));
    assert_eq!(detail.evidence, evidence);
    let entry = reopened
        .entries_of(row.artifact_id)?
        .into_iter()
        .next()
        .ok_or_else(|| IndexError::Catalog("a path".into()))?;
    assert_eq!(entry.sha256, Some(sha));
    assert_eq!(entry.git_sha1, Some(git));

    let json = serde_json::to_value(row).map_err(|error| IndexError::Catalog(error.to_string()))?;
    assert_eq!(
        json.get("blob_sha256").and_then(serde_json::Value::as_str),
        Some(sha.to_hex().as_str()),
        "JSON still spells a hash in hex"
    );
    assert_eq!(
        json.get("artifact_id").and_then(serde_json::Value::as_str),
        Some(row.artifact_id.to_string().as_str()),
        "JSON still spells a row id with its prefix"
    );
    let detail_json =
        serde_json::to_value(&detail).map_err(|error| IndexError::Catalog(error.to_string()))?;
    assert_eq!(
        detail_json
            .pointer("/tensors/samples/0/shape/2")
            .and_then(serde_json::Value::as_u64),
        Some(u64::MAX)
    );
    Ok(())
}

#[test]
fn cold_records_read_back_through_the_inspector() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let config = IndexConfig {
        data_dir: directory.path().join("data"),
        sources: Vec::new(),
        ..IndexConfig::default()
    };
    let native = config.native_path();
    let catalog = Catalog::open_or_scan(&config, "workstation", |_, _| {})?;
    let prose = "the same sentence over and over, which is what a card reads like. ".repeat(128);
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let noise: String = (0..4096)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            char::from(b'a' + (state >> 24) as u8 % 26)
        })
        .collect();
    let cards = [String::new(), prose.clone(), noise.clone()];
    let tensors = |index: usize| TensorSummary {
        tensor_count: 256,
        params: 12_345_678,
        dtypes: BTreeMap::from([("F16".to_owned(), 250), ("F32".to_owned(), 6)]),
        key_prefixes: (0..24)
            .map(|block| (format!("lora_unet_down_blocks_{block}_attentions_0"), 8))
            .collect(),
        samples: (0..8)
            .map(|slot| TensorSample {
                name: format!("lora_unet_down_blocks_{slot}_attn1_to_q.lora_down.weight"),
                dtype: "F16".to_owned(),
                shape: vec![32, 640],
            })
            .collect(),
        metadata: BTreeMap::from([
            ("format".to_owned(), "pt".to_owned()),
            ("ss_comment".to_owned(), format!("run {index}")),
        ]),
        ..Default::default()
    };
    let entry = |index: usize, card: &String| SeedEntry {
        node: LOCAL_NODE,
        rel_path: format!("w{index}.safetensors"),
        abs_path: Some(format!("/vault/w{index}.safetensors")),
        size: 1024 + index as u64,
        state: State::Local,
        is_symlink: false,
        sha256: None,
        git_sha1: None,
        artifact: Some(SeedArtifact {
            identity: 900 + index as u128,
            artifact: Artifact {
                id: ArtifactId::new(0),
                blob: BlobId::new(0),
                sha256: None,
                size: 1024 + index as u64,
                format: ArtifactFormat::Safetensors,
                role: Role::Lora,
                sub_role: None,
                role_confidence: 0.9,
                base: Base::new("sdxl"),
                base_label: None,
                base_confidence: 0.9,
                precision: None,
                quant: None,
                rank: None,
                alpha: None,
                params: None,
                context: None,
                trigger: None,
            },
            name: format!("w{index}.safetensors"),
            org: None,
            tensors: Some(tensors(index)),
            evidence: vec![Evidence {
                field: "role".to_owned(),
                value: "lora".to_owned(),
                source: EvidenceSource::Tensor,
                detail: Some(card.clone()),
                confidence: 0.9,
            }],
            card: Some(card.clone()),
            group_bytes: None,
            shard_count: None,
        }),
    };
    let repo = RepoId::parse_ref(RepoType::Models, "acme/cold")?;
    catalog.seed(&[SeedRepository {
        repo,
        source: "cache".to_owned(),
        kind: SourceKind::HfCache,
        root_path: "/vault".into(),
        git_dir: None,
        tool_layout: None,
        revisions: cards
            .iter()
            .enumerate()
            .map(|(index, card)| SeedRevision {
                name: format!("r{index}"),
                sha: None,
                refs: vec![(format!("r{index}"), RefKind::Branch)],
                title: None,
                author: None,
                committed: None,
                entries: vec![entry(index, card)],
            })
            .collect(),
    }])?;
    catalog.rebuild_search(|_| {})?;
    let ids: Vec<ArtifactId> = search(&catalog, "")?
        .rows
        .iter()
        .map(|row| row.artifact_id)
        .collect();
    assert_eq!(ids.len(), cards.len());
    let before: Vec<_> = ids
        .iter()
        .map(|id| catalog.artifact_detail(*id))
        .collect::<Result<Vec<_>>>()?;
    drop(catalog);

    let reopened = Catalog::open(&native)?;
    let after: Vec<_> = ids
        .iter()
        .map(|id| reopened.artifact_detail(*id))
        .collect::<Result<Vec<_>>>()?;
    assert_eq!(
        after, before,
        "an inspector view is the same whether the row was compressed or not"
    );
    for (index, detail) in after.iter().enumerate() {
        let detail = detail
            .as_ref()
            .ok_or_else(|| IndexError::Catalog(format!("no detail for artifact {index}")))?;
        assert_eq!(detail.tensors.as_ref(), Some(&tensors(index)));
        assert_eq!(
            detail
                .evidence
                .first()
                .and_then(|item| item.detail.as_ref()),
            Some(&cards[index]),
            "no card is truncated, whatever it compresses to"
        );
    }
    let found = search(&reopened, "sentence")?;
    assert_eq!(
        found.total, 1,
        "a compressed card is still what the search index was built from"
    );
    Ok(())
}

/// A LoRA header holding more tensors than a stored row keeps its samples of.
fn many_tensor_header(count: usize) -> Vec<u8> {
    let mut header = String::from(r#"{"__metadata__":{"ss_network_dim":"16"}"#);
    for index in 0..count {
        let start = index * 2048;
        header.push_str(&format!(
            ",\"lora_unet_block{index:03}_attn.lora_down.weight\":\
             {{\"dtype\":\"F16\",\"shape\":[16,64],\"data_offsets\":[{start},{}]}}",
            start + 2048
        ));
    }
    header.push('}');
    crate::sniff::tests::safetensors_fixture(&header)
}

#[test]
fn a_stored_detail_keeps_the_samples_the_inspector_shows_and_no_more() -> Result<()> {
    let tensors = 40;
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("acme");
    fs::create_dir(&input)?;
    fs::write(input.join("wide.safetensors"), many_tensor_header(tensors))?;
    let config = IndexConfig {
        sources: vec![source(&input, "loose")],
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
    let row = search(&catalog, "")?
        .rows
        .into_iter()
        .next()
        .ok_or_else(|| IndexError::Catalog("the fixture indexed no artifact".into()))?;
    assert_eq!(row.role, Role::Lora, "inference read the whole header");
    let header = catalog
        .artifact_detail(row.artifact_id)?
        .and_then(|detail| detail.tensors)
        .ok_or_else(|| IndexError::Catalog("the artifact carries no header".into()))?;

    assert_eq!(
        header.tensor_count, tensors as u32,
        "what the header proved is kept whole"
    );
    assert_eq!(
        header.params,
        tensors as u64 * 16 * 64,
        "so are the parameters it counted"
    );
    assert_eq!(
        header.dtypes.get("F16").copied(),
        Some(tensors as u64 * 16 * 64),
        "the dtype histogram counts elements, not tensors"
    );
    assert_eq!(
        header.metadata.get("ss_network_dim").map(String::as_str),
        Some("16"),
        "and the metadata the file carried"
    );
    let names: Vec<&str> = header
        .samples
        .iter()
        .map(|sample| sample.name.as_str())
        .collect();
    let expected: Vec<String> = (0..tensors)
        .step_by(tensors.div_ceil(16))
        .map(|index| format!("lora_unet_block{index:03}_attn.lora_down.weight"))
        .collect();
    assert_eq!(
        names, expected,
        "the bound thins the draw by a stride, so it spans the file rather than its first \
         quarter"
    );
    assert!(
        header.samples.len() <= 16,
        "and never more than sixteen of them, got {}",
        header.samples.len()
    );
    Ok(())
}

/// Every record the compact codec was turned on for, with the fields most likely to be
/// spelled differently by a varint encoding: a path, an enum, a float, a nested vector and
/// integers at the edge of their width.
#[test]
fn every_record_round_trips_through_the_compact_codec() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let config = IndexConfig {
        data_dir: directory.path().join("data"),
        sources: Vec::new(),
        ..IndexConfig::default()
    };
    let native = config.native_path();
    let catalog = Catalog::open_or_scan(&config, "workstation", |_, _| {})?;

    let repo = RepoId::parse_ref(RepoType::Models, "acme/round-trip")?;
    let source = Source {
        name: "codec".to_owned(),
        config: SourceConfig {
            name: "codec".to_owned(),
            path: PathBuf::from("/vault/models"),
            kind: SourceKind::HfCache,
            include: vec!["*.safetensors".to_owned()],
            exclude: vec!["*.tmp".to_owned(), "*.part".to_owned()],
            follow_symlinks: true,
            repo_per_folder: true,
        },
        resolved_kind: SourceKind::Loose,
        file_count: 1 << 40,
        byte_count: u64::MAX,
        last_scan: Some("2026-09-07T08:00:00Z".to_owned()),
        pending_hashes: 300,
        pending_bytes: 1 << 50,
    };
    let repository = Repository {
        id: 4242,
        key: models::repository_key("codec", &repo),
        source: "codec".to_owned(),
        repo: repo.clone(),
        root_path: "/vault/models/acme".to_owned(),
        git_dir: Some("/vault/models/acme/.git".to_owned()),
        default_ref: "main".to_owned(),
        remote: Some("https://example.invalid/acme/round-trip".to_owned()),
        tool_layout: Some("comfyui".to_owned()),
        revisions: vec![models::Revision {
            name: "v2".to_owned(),
            sha: Some(CommitSha::from_bytes([0x5a; 20])),
            refs: vec![models::Ref {
                name: "v2".to_owned(),
                kind: RefKind::Tag,
            }],
            title: Some("second".to_owned()),
            author: Some("someone".to_owned()),
            committed: Some("2026-09-06T10:00:00Z".to_owned()),
            retired: false,
        }],
        present_files: u32::MAX,
        present_bytes: 1 << 50,
    };
    let verdict = models::Override {
        artifact_id: 4243,
        role: Some(Role::Lora),
        sub_role: Some(SubRole::Ipa),
        base: Some(Base::new("sdxl")),
        decided_at: "2026-09-07T08:01:00Z".to_owned(),
    };
    let change = SearchChange {
        seq: u64::MAX - 3,
        artifact_id: 4243,
    };
    let summary = models::Summary {
        id: 3,
        reclaimable_bytes: 1 << 45,
        duplicate_artifacts: 12,
        artifacts: 1_000_000,
        present_artifacts: 999_999,
        triage_artifacts: 7,
        indexed_bytes: u64::MAX / 3,
        repositories: 4096,
        advertised_artifacts: 128,
    };
    let allocator = Allocator {
        id: 7,
        repos: 1 << 33,
        artifacts: 1 << 34,
        entries: 1 << 35,
    };
    let setting = models::Setting {
        key: "codec.round.trip".to_owned(),
        value: "dense".to_owned(),
    };
    let shelf = Shelf {
        id: 9001,
        name: "codec round trip".to_owned(),
        query: "role:lora sort:size".to_owned(),
        builtin: false,
        position: -3,
    };
    let noise = models::Noise {
        id: 9002,
        path: "/vault/noise".to_owned(),
        reason: Some("not weights".to_owned()),
        added_at: "2026-09-07T08:02:00Z".to_owned(),
    };
    let projection = models::Projection {
        id: 9003,
        link_path: "/tools/comfy/loras/round-trip.safetensors".to_owned(),
        profile: "comfy".to_owned(),
        target_path: "/vault/models/acme/round-trip.safetensors".to_owned(),
        artifact_id: Some(4243),
        written_at: "2026-09-07T08:03:00Z".to_owned(),
    };
    let usage = models::Usage {
        seq: 9004,
        repo_id: Some(4242),
        at: "2026-09-07T08:04:00Z".to_owned(),
        client: Some("10.0.0.9".to_owned()),
        artifact_id: Some(4243),
        path: "round-trip.safetensors".to_owned(),
        bytes: 1 << 40,
        rate_bps: Some(125_000_000),
        status: 206,
    };
    let repo_usage = models::RepoUsage {
        repo_id: 4242,
        pulls: 4096,
        days: vec![("2026-09-07".to_owned(), 12), ("2026-09-06".to_owned(), 30)],
        clients: vec!["10.0.0.9".to_owned(), "10.0.0.10".to_owned()],
        last_pull: "2026-09-07T08:04:00Z".to_owned(),
    };
    let artifact_use = models::ArtifactUse {
        artifact_id: 4243,
        count: 77,
        last_used: "2026-09-07T08:05:00Z".to_owned(),
    };
    let loadout = models::LoadoutDoc {
        id: 9006,
        name: "codec".to_owned(),
        document: "name = \"codec\"\n".to_owned(),
        created_at: "2026-09-07T08:06:00Z".to_owned(),
        updated_at: "2026-09-07T08:07:00Z".to_owned(),
    };
    let job = models::Job {
        id: 9007,
        kind: "scan".to_owned(),
        state: JobState::Failed,
        phase: Some("walking".to_owned()),
        subject: Some("codec".to_owned()),
        progress: 0.625,
        bytes: 1 << 41,
        total: Some(1 << 42),
        rate_bps: Some(9_000_000),
        error: Some("a root went away".to_owned()),
        started: "2026-09-07T08:08:00Z".to_owned(),
        finished: Some("2026-09-07T08:09:00Z".to_owned()),
    };
    let node = Node {
        id: 9008,
        node_id: "codec-peer".to_owned(),
        name: "peer".to_owned(),
        url: Some("http://10.0.0.10:7777".to_owned()),
        is_self: false,
        online: true,
        indexed_bytes: 1 << 46,
        last_seen: Some("2026-09-07T08:10:00Z".to_owned()),
        cursor: Some("2026-09-07T08:09:59Z".to_owned()),
        synced: Some("2026-09-07T08:10:01Z".to_owned()),
        pass: 19,
        filed: 65_536,
    };

    let transaction = catalog.database.rw_transaction()?;
    transaction.upsert(source.clone())?;
    transaction.upsert(repository.clone())?;
    transaction.upsert(verdict.clone())?;
    transaction.upsert(change.clone())?;
    transaction.upsert(summary.clone())?;
    transaction.upsert(allocator.clone())?;
    transaction.upsert(setting.clone())?;
    transaction.upsert(shelf.clone())?;
    transaction.upsert(noise.clone())?;
    transaction.upsert(projection.clone())?;
    transaction.upsert(usage.clone())?;
    transaction.upsert(repo_usage.clone())?;
    transaction.upsert(artifact_use.clone())?;
    transaction.upsert(loadout.clone())?;
    transaction.upsert(job.clone())?;
    transaction.upsert(node.clone())?;
    transaction.commit()?;
    drop(catalog);

    let reopened = Catalog::open(&native)?;
    let read = reopened.database.r_transaction()?;
    assert_eq!(read.get().primary("codec".to_owned())?, Some(source));
    assert_eq!(read.get().primary(4242_i64)?, Some(repository));
    assert_eq!(read.get().primary(4243_i64)?, Some(verdict));
    assert_eq!(read.get().primary(u64::MAX - 3)?, Some(change));
    assert_eq!(read.get().primary(3_u8)?, Some(summary));
    assert_eq!(read.get().primary(7_u8)?, Some(allocator));
    assert_eq!(
        read.get().primary("codec.round.trip".to_owned())?,
        Some(setting)
    );
    assert_eq!(read.get().primary(9001_i64)?, Some(shelf));
    assert_eq!(read.get().primary(9002_i64)?, Some(noise));
    assert_eq!(read.get().primary(9003_i64)?, Some(projection));
    assert_eq!(read.get().primary(9004_u64)?, Some(usage));
    assert_eq!(read.get().primary(4242_i64)?, Some(repo_usage));
    assert_eq!(read.get().primary(4243_i64)?, Some(artifact_use));
    assert_eq!(read.get().primary(9006_i64)?, Some(loadout));
    assert_eq!(read.get().primary(9007_i64)?, Some(job));
    assert_eq!(read.get().primary(9008_i64)?, Some(node));
    Ok(())
}

#[test]
fn lean_search_fields_preserve_filters_facets_and_sorting() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let config = IndexConfig {
        data_dir: directory.path().join("data"),
        sources: Vec::new(),
        ..IndexConfig::default()
    };
    let catalog = Catalog::open_or_scan(&config, "workstation", |_, _| {})?;
    let sha = |byte: u8| Sha256::from_bytes([byte; 32]);
    struct Shape {
        name: &'static str,
        role: Role,
        sub_role: Option<SubRole>,
        format: ArtifactFormat,
        base: &'static str,
        byte: u8,
    }
    let seeded = [
        Shape {
            name: "alpha",
            role: Role::Lora,
            sub_role: Some(SubRole::Ipa),
            format: ArtifactFormat::Safetensors,
            base: "sdxl 1.0",
            byte: 0x11,
        },
        Shape {
            name: "bravo",
            role: Role::Ckpt,
            sub_role: None,
            format: ArtifactFormat::Gguf,
            base: "flux",
            byte: 0x22,
        },
        Shape {
            name: "charlie",
            role: Role::Lora,
            sub_role: Some(SubRole::Tok),
            format: ArtifactFormat::Safetensors,
            base: "sd 1.5",
            byte: 0x33,
        },
        Shape {
            name: "delta",
            role: Role::Vae,
            sub_role: None,
            format: ArtifactFormat::Safetensors,
            base: "sdxl 1.0",
            byte: 0x44,
        },
    ];
    let entries: Vec<SeedEntry> = seeded
        .iter()
        .enumerate()
        .map(|(index, shape)| SeedEntry {
            node: LOCAL_NODE,
            rel_path: format!("{}.safetensors", shape.name),
            abs_path: Some(format!("/vault/{}.safetensors", shape.name)),
            size: 4096,
            state: State::Local,
            is_symlink: false,
            sha256: Some(sha(shape.byte)),
            git_sha1: None,
            artifact: Some(SeedArtifact {
                identity: 500 + index as u128,
                artifact: Artifact {
                    id: ArtifactId::new(0),
                    blob: BlobId::new(0),
                    sha256: Some(sha(shape.byte)),
                    size: 4096,
                    format: shape.format,
                    role: shape.role,
                    sub_role: shape.sub_role,
                    role_confidence: 0.99,
                    base: Base::new(shape.base),
                    base_label: Some(shape.base.to_uppercase()),
                    base_confidence: 0.99,
                    precision: Some(Precision::from_dtype("F16")),
                    quant: None,
                    rank: None,
                    alpha: None,
                    params: None,
                    context: None,
                    trigger: Some(format!("{} style, cinematic", shape.name)),
                },
                name: format!("{}.safetensors", shape.name),
                org: Some(format!("org-{}", shape.name)),
                tensors: None,
                evidence: Vec::new(),
                card: Some(format!("the {} adapter", shape.name)),
                group_bytes: None,
                shard_count: None,
            }),
        })
        .collect();
    let repo = RepoId::parse_ref(RepoType::Models, "acme/lean")?;
    catalog.seed(&[SeedRepository {
        repo,
        source: "cache".to_owned(),
        kind: SourceKind::HfCache,
        root_path: "/vault".into(),
        git_dir: None,
        tool_layout: None,
        revisions: vec![SeedRevision {
            name: "main".to_owned(),
            sha: Some(CommitSha::from_bytes([0x99; 20])),
            refs: vec![("main".to_owned(), RefKind::Branch)],
            title: None,
            author: None,
            committed: None,
            entries,
        }],
    }])?;
    catalog.rebuild_search(|_| {})?;

    for (query, expected) in [
        ("role:lora", 2),
        ("role:ip-adapter", 1),
        ("base:sdxl", 2),
        ("fmt:gguf", 1),
        ("prec:fp16", 4),
        ("state:local", 4),
        ("src:cache", 4),
        ("repo:lean", 4),
        ("rev:main", 4),
        ("org:org-alpha", 1),
        ("hash:1111111111111111", 1),
        ("trigger:cinematic", 4),
        ("trigger:alpha", 1),
        ("node:self", 4),
        ("dupe:false", 4),
        ("adapter", 4),
        ("~char", 1),
        ("size:>1b", 4),
    ] {
        assert_eq!(
            search(&catalog, query)?.total,
            expected,
            "{query} still matches what it matched"
        );
    }

    let faceted = search(&catalog, "")?;
    assert_eq!(faceted.facets.role.len(), 3, "role buckets are counted");
    assert_eq!(faceted.facets.base.len(), 3, "base buckets are counted");
    assert_eq!(faceted.facets.format.len(), 2);
    assert_eq!(faceted.facets.precision.len(), 1);
    assert_eq!(faceted.facets.node.len(), 1);

    let context = catalog.compile_context("", jiff::Timestamp::now(), Vec::new())?;
    for key in [
        SortKey::Role,
        SortKey::Name,
        SortKey::Base,
        SortKey::Format,
        SortKey::Precision,
        SortKey::Size,
        SortKey::Revision,
        SortKey::Node,
        SortKey::State,
        SortKey::Used,
        SortKey::Ident,
    ] {
        for desc in [false, true] {
            let page = catalog.query(QueryRequest {
                raw: "",
                sort: SortSpec::new(key, desc),
                limit: 50,
                offset: 0,
                context: &context,
            })?;
            assert_eq!(page.rows.len(), 4, "sorting by {key} keeps every row");
            let ids: Vec<i64> = page.rows.iter().map(|row| row.artifact_id.get()).collect();
            let mut sorted = ids.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(sorted.len(), 4, "sorting by {key} repeats no row: {ids:?}");
        }
    }
    let by_state = catalog.query(QueryRequest {
        raw: "",
        sort: SortSpec::new(SortKey::State, false),
        limit: 50,
        offset: 0,
        context: &context,
    })?;
    let ids: Vec<i64> = by_state
        .rows
        .iter()
        .map(|row| row.artifact_id.get())
        .collect();
    let mut ascending = ids.clone();
    ascending.sort_unstable();
    assert_eq!(
        ids, ascending,
        "rows that tie on the sort column fall back to artifact id"
    );
    let labels: Vec<Option<String>> = by_state
        .rows
        .iter()
        .map(|row| row.base_label.clone())
        .collect();
    assert!(
        labels.iter().all(Option::is_some),
        "the base label is still read out of its column: {labels:?}"
    );
    Ok(())
}
