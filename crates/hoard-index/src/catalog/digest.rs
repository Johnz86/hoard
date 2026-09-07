use native_db::transaction::RwTransaction;

use hoard_core::{
    Artifact, ArtifactFormat, ArtifactId, Base, BlobId, CommitSha, RepoId, Role, Sha256, State,
};

use crate::config::{IndexConfig, SourceConfig, SourceKind};
use crate::error::{IndexError, Result};
use crate::sniff::SNIFF_VERSION;

use super::Catalog;
use super::hashing::content_key;
use super::models::{
    ArtifactDetail, ArtifactRecord, ArtifactRecordKey, Entry, EntryKey, LOCAL_NODE, LocationKey,
    Node, Placement, Repository, RepositoryKey, Revision, SearchText, Source, entry_at,
    repository_key,
};
use super::nodes::PEER_SOURCE;
use super::pulls::PULL_SOURCE;
use super::read::Reader;
use super::scan::{
    CHUNK, Counters, log_changes, next_id, preference, preferred_entry, sweep_artifact,
};

/// One artifact of this node, flattened to what a peer needs to find and fetch it.
#[derive(Debug, Clone, PartialEq)]
pub struct DigestEntry {
    pub sha256: Sha256,
    pub size: u64,
    pub role: Role,
    pub base: Base,
    pub repo: String,
    pub revision_sha: Option<CommitSha>,
    pub path: String,
    pub state: State,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DigestPage {
    pub rows: Vec<DigestEntry>,
    /// The highest artifact row this page reached, which is where the next one starts.
    /// A page that returned nothing has nowhere new to point, so a reader that asks again
    /// from the same place is asking for what has been added since.
    pub next: Option<i64>,
    pub total: u64,
}

/// A peer's row, parsed and validated, ready to be filed against that peer's node row.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteEntry {
    pub sha256: Sha256,
    pub size: u64,
    pub role: Role,
    pub base: Base,
    pub repo: RepoId,
    pub revision_sha: CommitSha,
    pub path: String,
    pub name: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DigestApplied {
    pub entries: u64,
    pub artifacts: u64,
}

impl Catalog {
    /// This node's hashed artifacts with an id above `since`, in id order, each as its
    /// preferred local entry; `total` is how many such artifacts the catalog holds.
    pub fn digest(&self, since: Option<i64>, limit: u32) -> Result<DigestPage> {
        let wanted = limit.max(1) as usize;
        let total = self.totals()?.advertised_artifacts;
        let snapshot = self.database.r_transaction()?;
        let names = self.node_names(&snapshot)?;
        let mut reader = Reader::new(&snapshot, &names);
        let start = since.map_or(0, |id| id.saturating_add(1)).max(0);
        let scan = snapshot.scan();
        let by_id = scan.primary::<ArtifactRecord>()?;
        let mut rows = Vec::with_capacity(wanted.min(64));
        let mut next = None;
        for record in by_id.range(start..)? {
            if rows.len() == wanted {
                break;
            }
            let record = record?;
            if record.artifact.sha256.is_none() {
                continue;
            }
            let Some(entry) = preferred_entry(reader.entries_of_artifact(record.id)?)
                .filter(|entry| entry.node == LOCAL_NODE && entry.state != State::Remote)
            else {
                continue;
            };
            let row = reader.row_of(&record, &entry, None)?;
            let Some(sha256) = row.blob_sha256 else {
                continue;
            };
            next = Some(record.id);
            rows.push(DigestEntry {
                sha256,
                size: row.size,
                role: row.role,
                base: row.base,
                repo: row.repo,
                revision_sha: row.revision_sha,
                path: row.path,
                state: row.state,
            });
        }
        Ok(DigestPage { rows, next, total })
    }

    /// Files a peer's rows as remote entries owned by that peer, stamped with `pass`.
    ///
    /// Bytes this node already knows keep their own record: only the entry saying the peer
    /// also holds them is added, which is what makes one hash show every node that has it.
    pub fn apply_digest(
        &self,
        peer: i64,
        rows: &[RemoteEntry],
        pass: u64,
    ) -> Result<DigestApplied> {
        let mut counters =
            Counters::load(&self.database, IndexConfig::default().triage_confidence)?;
        let transaction = self.database.rw_transaction()?;
        let node: Node = transaction
            .get()
            .primary(peer)?
            .ok_or_else(|| IndexError::Catalog(format!("node {peer} is not a known peer")))?;
        let applied = {
            let mut applying = Applying {
                transaction: &transaction,
                counters: &mut counters,
                source: format!("{PEER_SOURCE}{}", node.node_id),
                root: node.url.unwrap_or(node.node_id),
                peer,
                pass,
            };
            applying.source()?;
            let mut applied = DigestApplied::default();
            for row in rows {
                applied = applying.row(row, applied)?;
            }
            applied
        };
        log_changes(&transaction, &mut counters)?;
        counters.save(&transaction)?;
        transaction.commit()?;
        self.summarise(&counters)?;
        Ok(applied)
    }

    /// Starts a full pass over a peer's digest: bumps and returns the peer's pass number.
    pub fn begin_pass(&self, peer: i64) -> Result<u64> {
        let transaction = self.database.rw_transaction()?;
        let stored: Node = transaction
            .get()
            .primary(peer)?
            .ok_or_else(|| IndexError::Catalog(format!("node {peer} is not a known peer")))?;
        let pass = stored.pass.saturating_add(1);
        transaction.upsert(Node { pass, ..stored })?;
        transaction.commit()?;
        Ok(pass)
    }

    /// Drops the peer's remote entries an earlier pass stamped, and every artifact record
    /// nothing references any more. Returns how many entries went.
    ///
    /// The peer's rows are found through the repositories filed under its own source, one
    /// repository at a time and in chunks of `CHUNK` rows per transaction, so neither the
    /// pass nor an index by node has to see every entry this catalog holds.
    pub fn retract_digest(&self, peer: i64, pass: u64) -> Result<u64> {
        let stored: Option<Node> = self.database.r_transaction()?.get().primary(peer)?;
        let node = stored
            .ok_or_else(|| IndexError::Catalog(format!("node {peer} is not a known peer")))?;
        let source = format!("{PEER_SOURCE}{}", node.node_id);
        let mut counters =
            Counters::load(&self.database, IndexConfig::default().triage_confidence)?;
        let mut artifacts = Vec::new();
        let mut removed = 0;
        for repo_id in self.repositories_of_source(&source)? {
            let stale = self.entries_of_earlier_passes(repo_id, peer, pass)?;
            for batch in stale.chunks(CHUNK) {
                let transaction = self.database.rw_transaction()?;
                for entry in batch {
                    if let Some(id) = entry.artifact_id {
                        artifacts.push(id);
                    }
                    counters.derived.removed(entry);
                    transaction.remove(entry.clone())?;
                    removed += 1;
                }
                log_changes(&transaction, &mut counters)?;
                counters.save(&transaction)?;
                transaction.commit()?;
            }
        }
        artifacts.sort_unstable();
        artifacts.dedup();
        for batch in artifacts.chunks(CHUNK) {
            let transaction = self.database.rw_transaction()?;
            for id in batch {
                counters.touch(&transaction, *id)?;
                if retract_artifact(&transaction, *id)? {
                    counters.pending.push(*id);
                }
            }
            log_changes(&transaction, &mut counters)?;
            counters.save(&transaction)?;
            transaction.commit()?;
        }
        if removed > 0 {
            self.summarise(&counters)?;
        }
        Ok(removed)
    }

    /// The peer's remote rows in one repository that an earlier pass stamped.
    fn entries_of_earlier_passes(&self, repo_id: i64, peer: i64, pass: u64) -> Result<Vec<Entry>> {
        let read = self.database.r_transaction()?;
        let rows: Vec<Entry> = read
            .scan()
            .secondary::<Entry>(EntryKey::location)?
            .start_with(LocationKey::repository(repo_id))?
            .collect::<native_db::db_type::Result<_>>()?;
        let mut stale: Vec<Entry> = rows
            .into_iter()
            .filter(|entry| {
                entry.repo_id == repo_id
                    && entry.node == peer
                    && entry.state == State::Remote
                    && entry.pass != pass
            })
            .collect();
        stale.sort_unstable_by_key(|entry| entry.id);
        Ok(stale)
    }

    pub fn peer_cursor(&self, peer: i64) -> Result<Option<String>> {
        let stored: Option<Node> = self.database.r_transaction()?.get().primary(peer)?;
        Ok(stored.and_then(|node| node.cursor))
    }

    /// Records how far this node has read a peer's digest. `None` keeps the cursor already
    /// stored and only refreshes `synced`, so a page that ends the digest leaves the last
    /// position standing.
    pub fn write_peer_cursor(
        &self,
        peer: i64,
        cursor: Option<&str>,
        at: jiff::Timestamp,
    ) -> Result<()> {
        let transaction = self.database.rw_transaction()?;
        let stored: Node = transaction
            .get()
            .primary(peer)?
            .ok_or_else(|| IndexError::Catalog(format!("node {peer} is not a known peer")))?;
        transaction.upsert(Node {
            cursor: cursor.map(str::to_owned).or(stored.cursor),
            synced: Some(at.to_string()),
            ..stored
        })?;
        transaction.commit()?;
        Ok(())
    }

    /// How many entries this node files for a peer, as the writers that filed them left
    /// the peer's count.
    pub fn filed_for(&self, peer: i64) -> Result<u64> {
        let stored: Option<Node> = self.database.r_transaction()?.get().primary(peer)?;
        Ok(stored.map_or(0, |node| node.filed))
    }

    /// Returns every entry a stopped process left mid-transfer to REMOTE, which is what
    /// clears a percentage that will never move again.
    ///
    /// Only a pull writes PULLING, so only the repositories filed under the pull source are
    /// read, in chunks of `CHUNK` rows per transaction.
    pub fn reset_stale_pulls(&self) -> Result<u64> {
        let mut counters =
            Counters::load(&self.database, IndexConfig::default().triage_confidence)?;
        let mut reset = 0;
        for repo_id in self.repositories_of_source(PULL_SOURCE)? {
            let pulling = self.entries_in_state(repo_id, State::Pulling)?;
            for batch in pulling.chunks(CHUNK) {
                let transaction = self.database.rw_transaction()?;
                for entry in batch {
                    if let Some(id) = entry.artifact_id {
                        counters.pending.push(id);
                    }
                    counters.derived.removed(entry);
                    transaction.upsert(Entry {
                        state: State::Remote,
                        preference: preference(State::Remote, false),
                        pull_progress: None,
                        ..entry.clone()
                    })?;
                    reset += 1;
                }
                log_changes(&transaction, &mut counters)?;
                counters.save(&transaction)?;
                transaction.commit()?;
            }
        }
        Ok(reset)
    }

    /// One repository's entries in one state.
    fn entries_in_state(&self, repo_id: i64, state: State) -> Result<Vec<Entry>> {
        let read = self.database.r_transaction()?;
        let rows: Vec<Entry> = read
            .scan()
            .secondary::<Entry>(EntryKey::location)?
            .start_with(LocationKey::repository(repo_id))?
            .collect::<native_db::db_type::Result<_>>()?;
        let mut matched: Vec<Entry> = rows
            .into_iter()
            .filter(|entry| entry.repo_id == repo_id && entry.state == state)
            .collect();
        matched.sort_unstable_by_key(|entry| entry.id);
        Ok(matched)
    }
}

/// One `apply_digest` call in progress: the transaction every record lands in, the allocator
/// that numbers the rows it creates, and the peer whose namespace they belong to.
struct Applying<'a, 'db> {
    transaction: &'a RwTransaction<'db>,
    counters: &'a mut Counters,
    source: String,
    root: String,
    peer: i64,
    pass: u64,
}

impl Applying<'_, '_> {
    /// The namespace a peer's repositories sit under, so nothing it advertises collides with
    /// what a scan of this disk found under the same qualified name.
    fn source(&mut self) -> Result<()> {
        let stored: Option<Source> = self.transaction.get().primary(self.source.clone())?;
        if stored.is_some() {
            return Ok(());
        }
        self.transaction.insert(Source {
            name: self.source.clone(),
            config: SourceConfig {
                name: self.source.clone(),
                path: self.root.clone().into(),
                kind: SourceKind::Loose,
                include: Vec::new(),
                exclude: Vec::new(),
                follow_symlinks: false,
                repo_per_folder: false,
            },
            resolved_kind: SourceKind::Loose,
            file_count: 0,
            byte_count: 0,
            last_scan: None,
            pending_hashes: 0,
        })?;
        Ok(())
    }

    fn row(&mut self, row: &RemoteEntry, mut applied: DigestApplied) -> Result<DigestApplied> {
        let repo_id = self.repository(row)?;
        let ordinal = self.revision(repo_id, row)?;
        let identity = content_key(&row.sha256);
        let (artifact_id, created) = self.artifact(row, identity)?;
        if created {
            applied.artifacts += 1;
        }
        let previous = entry_at(self.transaction, repo_id, ordinal, &row.path)?;
        let id = match &previous {
            Some(entry) => entry.id,
            None => next_id(&mut self.counters.entries)?,
        };
        if let Some(previous) = &previous {
            self.counters.derived.removed(previous);
        }
        let entry = Entry {
            id,
            repo_id,
            artifact_id: Some(artifact_id),
            revision: ordinal,
            rel_path: row.path.clone(),
            placed: Placement::Absent,
            size: row.size,
            state: State::Remote,
            triage: false,
            preference: preference(State::Remote, false),
            lfs_pointer: false,
            is_symlink: false,
            sha256: Some(row.sha256),
            content: Some(identity),
            git_sha1: None,
            mtime: None,
            inode: None,
            node: self.peer,
            pass: self.pass,
            pull_progress: None,
        };
        self.counters.derived.added(&entry);
        self.transaction.upsert(entry)?;
        applied.entries += 1;
        Ok(applied)
    }

    fn repository(&mut self, row: &RemoteEntry) -> Result<i64> {
        let key = repository_key(&self.source, &row.repo);
        let stored: Option<Repository> = self
            .transaction
            .get()
            .secondary(RepositoryKey::key, key.as_str())?;
        if let Some(stored) = stored {
            return Ok(stored.id);
        }
        let id = self.counters.next_repo()?;
        self.transaction.insert(Repository {
            id,
            key,
            source: self.source.clone(),
            repo: row.repo.clone(),
            root_path: self.root.clone(),
            git_dir: None,
            default_ref: crate::discover::DEFAULT_REF.to_owned(),
            remote: None,
            revisions: Vec::new(),
            present_files: 0,
            present_bytes: 0,
        })?;
        Ok(id)
    }

    /// The ordinal of the revision the row names, appended when the peer's repository does
    /// not hold it yet. A peer that named none reports a hash-derived one, so two nodes
    /// agree on the same synthetic revision for the same bytes.
    fn revision(&mut self, repo_id: i64, row: &RemoteEntry) -> Result<u32> {
        let stored: Repository = self
            .transaction
            .get()
            .primary(repo_id)?
            .ok_or_else(|| IndexError::Catalog(format!("repository {repo_id} vanished")))?;
        if let Some(ordinal) = stored
            .revisions
            .iter()
            .position(|revision| revision.sha == Some(row.revision_sha))
        {
            return ordinal_of(ordinal);
        }
        let ordinal = ordinal_of(stored.revisions.len())?;
        let mut revisions = stored.revisions.clone();
        revisions.push(Revision {
            name: row.revision_sha.short(),
            sha: Some(row.revision_sha),
            refs: Vec::new(),
            title: None,
            author: None,
            committed: None,
        });
        self.transaction.upsert(Repository {
            revisions,
            ..stored
        })?;
        Ok(ordinal)
    }

    /// The record these bytes belong to, shared with any copy this node already knows by
    /// content, and said to have been written by this call when it was.
    fn artifact(&mut self, row: &RemoteEntry, identity: u128) -> Result<(i64, bool)> {
        let stored: Option<ArtifactRecord> = self
            .transaction
            .get()
            .secondary(ArtifactRecordKey::identity, identity)?;
        if let Some(stored) = stored {
            self.touch(stored.id)?;
            return Ok((stored.id, false));
        }
        let id = next_id(&mut self.counters.artifacts)?;
        self.touch(id)?;
        self.transaction.upsert(SearchText {
            artifact_id: id,
            keys: String::new(),
            card: String::new(),
        })?;
        self.transaction.upsert(ArtifactDetail {
            artifact_id: id,
            tensors: None,
            evidence: Vec::new(),
        })?;
        self.transaction.upsert(ArtifactRecord {
            id,
            identity,
            artifact: Artifact {
                id: ArtifactId::new(id),
                blob: BlobId::new(id),
                sha256: Some(row.sha256),
                size: row.size,
                format: ArtifactFormat::Unknown,
                role: row.role,
                sub_role: None,
                role_confidence: 0.0,
                base: row.base.clone(),
                base_label: None,
                base_confidence: 0.0,
                precision: None,
                quant: None,
                rank: None,
                alpha: None,
                params: None,
                context: None,
                trigger: None,
                sniff_version: SNIFF_VERSION,
            },
            name: row.name.clone(),
            org: row.repo.org().map(str::to_owned),
            path_count: 0,
            group_bytes: None,
            shard_count: None,
            triage_cleared: false,
            first_seen: jiff::Timestamp::now().to_string(),
        })?;
        Ok((id, true))
    }

    fn touch(&mut self, id: i64) -> Result<()> {
        self.counters.touch(self.transaction, id)?;
        self.counters.pending.push(id);
        Ok(())
    }
}

/// What a retraction leaves of the record its entries pointed at. An artifact no entry
/// reaches at all is gone with them; one another node still offers keeps its record, which
/// is the difference between a peer dropping a file and this node forgetting the bytes.
fn retract_artifact(transaction: &RwTransaction<'_>, id: i64) -> Result<bool> {
    let referencing: Vec<Entry> = transaction
        .scan()
        .secondary::<Entry>(EntryKey::artifact_id)?
        .start_with(Some(id))?
        .collect::<native_db::db_type::Result<_>>()?;
    let held = referencing
        .iter()
        .any(|entry| entry.state != State::Remote && entry.artifact_id == Some(id));
    match referencing.is_empty() || held {
        true => sweep_artifact(transaction, id),
        false => Ok(false),
    }
}

fn ordinal_of(position: usize) -> Result<u32> {
    u32::try_from(position).map_err(|_| IndexError::Catalog("revision ordinal overflow".into()))
}
