//! LSM storage engine combining WAL, memtable, SSTs, and manifest.
//!
//! # Architecture
//!
//! The [`Engine`] is the top-level row-store interface. It coordinates:
//! - An active [`Memtable`] for current writes.
//! - A list of immutable [`Memtable`]s pending flush.
//! - An ordered (newest-first) list of immutable on-disk [`SstReader`]s.
//! - A write-ahead log ([`Wal`]) providing durability across crashes.
//! - An authoritative [`Manifest`] tracking live SST files.
//!
//! # Lock Discipline and Concurrency
//!
//! Interior mutability and concurrency are governed by two levels of locks:
//! 1. `commit_lock: Mutex<CommitState>`: Serializes transaction commits, version
//!    assignments, WAL appends, and SST flushes. Only one thread can commit or
//!    flush at a time.
//! 2. `read_state: RwLock<ReadState>`: Protects the read path (active memtable,
//!    immutable memtables, and SST reader handles).
//!
//! ### Lock Ordering
//!
//! To prevent deadlocks, the engine strictly enforces:
//! - `commit_lock` is ALWAYS acquired before `read_state` write lock.
//! - `commit_lock` is NEVER acquired while holding any `read_state` lock (read or write).
//! - Point lookups ([`Engine::get`]) and snapshot reads acquire ONLY `read_state.read()`,
//!   allowing concurrent, contention-free reads across multiple threads.
//! - Write transactions ([`Engine::commit`]) hold `commit_lock` while performing the
//!   first-writer-wins conflict check and WAL append/fsync. During disk fsync, `read_state`
//!   is completely unlocked so readers are not blocked by disk I/O. Once the WAL append
//!   is durable, the writer briefly acquires `read_state.write()` to apply mutations to
//!   the active memtable and update `visible_version`.
//!
//! # Tombstone Contract & Delete Resurrection
//!
//! Deletions insert tombstones ([`ValueKind::Delete`]). When resolving a point lookup
//! across LSM layers (active memtable -> immutable memtables -> SSTs), encountering a
//! tombstone immediately yields `Ok(None)` and terminates search. Older layers are never
//! consulted, preventing resurrection of deleted rows.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use htap_common::{HtapError, Result, Row, Version};
use parking_lot::{Mutex, RwLock};

use crate::manifest::{sync_dir, Manifest, ManifestSstEntry};
use crate::memtable::{Memtable, ValueKind};
use crate::sst::{SstOptions, SstReader, SstWriter};
use crate::wal::{Wal, WalOptions, WalRecord};

/// Default memtable capacity in bytes before triggering an automatic flush (4 MiB).
pub const DEFAULT_MEMTABLE_BYTES: usize = 4 * 1024 * 1024;

/// Point-in-time MVCC snapshot version for isolation queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    /// MVCC commit version of this snapshot.
    pub version: Version,
}

impl Snapshot {
    /// Create a new snapshot at the given version.
    pub const fn new(version: Version) -> Self {
        Self { version }
    }
}

impl From<Version> for Snapshot {
    fn from(version: Version) -> Self {
        Self { version }
    }
}

/// A write mutation on a single row key within a partition.
#[derive(Debug, Clone, PartialEq)]
pub enum Mutation {
    /// Insert or update a row.
    Put {
        /// Target partition identifier.
        partition_id: u64,
        /// Primary key bytes.
        key: Vec<u8>,
        /// Row value.
        row: Row,
    },
    /// Delete a row (leaves an MVCC tombstone).
    Delete {
        /// Target partition identifier.
        partition_id: u64,
        /// Primary key bytes.
        key: Vec<u8>,
    },
}

/// Configuration options for the LSM [`Engine`].
#[derive(Debug, Clone)]
pub struct EngineOptions {
    /// Base directory holding WAL segments, SSTs, and MANIFEST.
    pub dir: PathBuf,
    /// Threshold in bytes before the active memtable is flushed to an SST.
    pub memtable_bytes: usize,
    /// SST creation options (target block size, bloom filter settings).
    pub sst: SstOptions,
    /// Write-ahead log options (segment size, sync-on-commit).
    pub wal: WalOptions,
}

impl EngineOptions {
    /// Create options with defaults for an engine rooted at `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let wal_dir = dir.join("wal");
        Self {
            sst: SstOptions::new(),
            wal: WalOptions::new(wal_dir),
            dir,
            memtable_bytes: DEFAULT_MEMTABLE_BYTES,
        }
    }

    /// Set the memtable flush threshold in bytes.
    #[must_use]
    pub fn with_memtable_bytes(mut self, bytes: usize) -> Self {
        self.memtable_bytes = bytes;
        self
    }

    /// Set options used when generating SST files.
    #[must_use]
    pub fn with_sst_options(mut self, options: SstOptions) -> Self {
        self.sst = options;
        self
    }

    /// Set options used for write-ahead logging.
    #[must_use]
    pub fn with_wal_options(mut self, mut options: WalOptions) -> Self {
        if options.dir.as_os_str().is_empty() {
            options.dir = self.dir.join("wal");
        }
        self.wal = options;
        self
    }
}

/// Internal commit state protected by `commit_lock`.
#[derive(Debug)]
struct CommitState {
    wal: Wal,
    next_sst_id: u64,
    manifest: Manifest,
}

/// Internal read state protected by `read_state` RwLock.
#[derive(Debug)]
struct ReadState {
    active: Memtable,
    immutables: Vec<Arc<Memtable>>,
    ssts: Vec<Arc<SstReader>>,
    visible_version: Version,
}

/// LSM row store engine.
#[derive(Debug)]
pub struct Engine {
    options: EngineOptions,
    commit_lock: Mutex<CommitState>,
    read_state: RwLock<ReadState>,
}

impl Engine {
    /// Open or recover an LSM engine at the configured directory.
    ///
    /// # Recovery Sequence
    ///
    /// 1. Create `<dir>`, `<dir>/wal/`, and `<dir>/sst/` as needed.
    /// 2. Read and validate `MANIFEST` (if absent, start empty). Open every listed
    ///    SST file; listed-but-missing or corrupted SSTs return an error.
    /// 3. Best-effort delete orphan `*.tmp` files and unlisted `.sst` files.
    /// 4. Open the WAL (repairing torn tails) and replay records.
    /// 5. Validate that all data records of committed transactions match their commit version.
    /// 6. Apply only committed transactions to the active memtable.
    /// 7. Set `visible_version = max(SST max_versions, replayed commit versions)`.
    pub fn open(options: EngineOptions) -> Result<Self> {
        std::fs::create_dir_all(&options.dir)?;
        let wal_dir = options.dir.join("wal");
        std::fs::create_dir_all(&wal_dir)?;
        let sst_dir = options.dir.join("sst");
        std::fs::create_dir_all(&sst_dir)?;

        // 2. Read and validate MANIFEST
        let manifest_path = options.dir.join("MANIFEST");
        let manifest = Manifest::read_from_file(&manifest_path)?.unwrap_or_default();

        let mut sst_readers = Vec::with_capacity(manifest.ssts.len());
        let mut max_sst_id = 0u64;

        for sst_meta in &manifest.ssts {
            max_sst_id = max_sst_id.max(sst_meta.id);
            let sst_path = sst_dir.join(format!("{}.sst", sst_meta.id));
            let reader = SstReader::open(&sst_path)?;
            if reader.metadata().id != sst_meta.id {
                return Err(HtapError::Corruption(format!(
                    "manifest SST id {} does not match SST file metadata id {}",
                    sst_meta.id,
                    reader.metadata().id
                )));
            }
            sst_readers.push(Arc::new(reader));
        }
        let next_sst_id = max_sst_id + 1;

        // 3. Best-effort delete orphan *.tmp and unlisted *.sst
        let _ = std::fs::remove_file(options.dir.join("MANIFEST.tmp"));
        let valid_sst_ids: HashSet<u64> = manifest.ssts.iter().map(|s| s.id).collect();
        if let Ok(entries) = std::fs::read_dir(&sst_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
                    if file_name.ends_with(".tmp") {
                        let _ = std::fs::remove_file(&path);
                    } else if let Some(stem) = file_name.strip_suffix(".sst") {
                        if let Ok(id) = stem.parse::<u64>() {
                            if !valid_sst_ids.contains(&id) {
                                let _ = std::fs::remove_file(&path);
                            }
                        } else {
                            let _ = std::fs::remove_file(&path);
                        }
                    }
                }
            }
        }

        // 4. Open WAL (repairs torn tail) and replay
        let wal = Wal::open(options.wal.clone())?;
        let replay = Wal::replay(&options.wal.dir)?;

        // 5 & 6. Validate and apply committed records
        let mut commit_versions = std::collections::HashMap::new();
        let mut aborted = HashSet::new();

        for (_, rec) in &replay.records {
            match rec {
                WalRecord::Commit { txn_id, version } => {
                    if let Some(prev) = commit_versions.insert(*txn_id, *version) {
                        if prev != *version {
                            return Err(HtapError::Corruption(format!(
                                "conflicting commit versions for transaction {txn_id}: {prev} vs {version}"
                            )));
                        }
                    }
                }
                WalRecord::Abort { txn_id } => {
                    aborted.insert(*txn_id);
                }
                _ => {}
            }
        }

        for aborted_id in &aborted {
            commit_versions.remove(aborted_id);
        }

        for (_, rec) in &replay.records {
            match rec {
                WalRecord::Put {
                    txn_id, version, ..
                }
                | WalRecord::Delete {
                    txn_id, version, ..
                } => {
                    if let Some(expected_v) = commit_versions.get(txn_id) {
                        if version != expected_v {
                            return Err(HtapError::Corruption(format!(
                                "transaction {txn_id} data record version {version} does not match commit version {expected_v}"
                            )));
                        }
                    }
                }
                _ => {}
            }
        }

        let mut active = Memtable::new();
        for (_, rec) in replay.committed_records() {
            match rec {
                WalRecord::Put {
                    partition_id,
                    key,
                    row,
                    version,
                    ..
                } => {
                    active.apply(partition_id, key, version, ValueKind::Put(row))?;
                }
                WalRecord::Delete {
                    partition_id,
                    key,
                    version,
                    ..
                } => {
                    active.apply(partition_id, key, version, ValueKind::Delete)?;
                }
                _ => {}
            }
        }

        // 7. visible_version = max(SST max_versions, replayed commit versions)
        let mut visible_version = Version::INITIAL;
        for sst_meta in &manifest.ssts {
            if let Some(v) = sst_meta.max_version {
                visible_version = visible_version.max(v);
            }
        }
        for v in commit_versions.values() {
            visible_version = visible_version.max(*v);
        }

        let commit_state = CommitState {
            wal,
            next_sst_id,
            manifest,
        };

        let read_state = ReadState {
            active,
            immutables: Vec::new(),
            ssts: sst_readers,
            visible_version,
        };

        Ok(Self {
            options,
            commit_lock: Mutex::new(commit_state),
            read_state: RwLock::new(read_state),
        })
    }

    /// Return the current MVCC snapshot.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            version: self.visible_version(),
        }
    }

    /// Return the current highest visible commit version.
    pub fn visible_version(&self) -> Version {
        self.read_state.read().visible_version
    }

    /// Retrieve a row by partition ID and user key at the given snapshot version.
    ///
    /// # R5 Fast Path
    ///
    /// This method touches ONLY row-store primary key structures. There is no SQL,
    /// planner, DataFusion, columnar, or plan-fragment code involved. There is no
    /// redundant secondary hash index — the ordered memtable and SST block index
    /// themselves ARE the primary key index.
    ///
    /// # Search Order and Tombstone Discipline
    ///
    /// Lookups check sources strictly newest-first:
    /// 1. Active memtable
    /// 2. Immutable memtables (newest first)
    /// 3. Published SST readers (newest first)
    ///
    /// At the *first* source returning `Some(entry)`:
    /// - If `ValueKind::Put(row)`: return `Ok(Some(row))`
    /// - If `ValueKind::Delete`: return `Ok(None)` and **STOP immediately**. Older
    ///   sources are never consulted, preventing deleted rows from resurrecting.
    ///
    /// Only if a source returns `None` does search continue to older sources.
    pub fn get(&self, partition_id: u64, key: &[u8], snapshot: Snapshot) -> Result<Option<Row>> {
        let read_guard = self.read_state.read();

        // 1. Active memtable
        if let Some(entry) = read_guard.active.get(partition_id, key, snapshot.version) {
            return match entry.value {
                ValueKind::Put(row) => Ok(Some(row)),
                ValueKind::Delete => Ok(None),
            };
        }

        // 2. Immutable memtables (newest first)
        for imm in &read_guard.immutables {
            if let Some(entry) = imm.get(partition_id, key, snapshot.version) {
                return match entry.value {
                    ValueKind::Put(row) => Ok(Some(row)),
                    ValueKind::Delete => Ok(None),
                };
            }
        }

        // 3. SST readers (newest first)
        for sst in &read_guard.ssts {
            if let Some(entry) = sst.get(partition_id, key, snapshot.version)? {
                return match entry.value {
                    ValueKind::Put(row) => Ok(Some(row)),
                    ValueKind::Delete => Ok(None),
                };
            }
        }

        Ok(None)
    }

    /// Commit a batch of mutations under Snapshot Isolation.
    ///
    /// # Commit Protocol
    ///
    /// 1. Reject empty batches and duplicate `(partition_id, key)` pairs within the batch.
    /// 2. **Conflict Check (first-writer-wins):** Find the newest committed version across
    ///    active memtable, immutable memtables, and SSTs. If any mutated key has a version
    ///    `> snapshot.version`, abort with [`HtapError::Conflict`] without writing to the WAL.
    /// 3. Assign `commit_version = visible_version.next()`.
    /// 4. Append mutations and commit record to WAL, then fsync.
    /// 5. Apply mutations to active memtable and advance `visible_version = commit_version`.
    /// 6. If active memtable size exceeds `memtable_bytes`, trigger a flush.
    pub fn commit(
        &self,
        txn_id: u64,
        snapshot: Snapshot,
        mutations: Vec<Mutation>,
    ) -> Result<Version> {
        let mut commit_guard = self.commit_lock.lock();

        // 1. Validate batch
        if mutations.is_empty() {
            return Err(HtapError::InvalidArgument(
                "mutation batch cannot be empty".into(),
            ));
        }

        let mut seen_keys = HashSet::with_capacity(mutations.len());
        for m in &mutations {
            let (pid, key) = match m {
                Mutation::Put {
                    partition_id, key, ..
                } => (*partition_id, key.as_slice()),
                Mutation::Delete { partition_id, key } => (*partition_id, key.as_slice()),
            };
            if !seen_keys.insert((pid, key)) {
                return Err(HtapError::InvalidArgument(format!(
                    "duplicate key in mutation batch: partition {pid}, key {key:?}"
                )));
            }
        }

        // 2. Conflict check (first-writer-wins)
        {
            let read_guard = self.read_state.read();
            for &(partition_id, key) in &seen_keys {
                if let Some(newest_version) =
                    Self::find_newest_version(&read_guard, partition_id, key)?
                {
                    if newest_version > snapshot.version {
                        return Err(HtapError::Conflict(format!(
                            "write-write conflict on partition {partition_id}, key {key:?}: newest committed version {newest_version} > snapshot {}",
                            snapshot.version
                        )));
                    }
                }
            }
        }

        // 3. Assign commit version
        let commit_version = self.read_state.read().visible_version.next();

        // 4. Append mutations and commit record to WAL
        for m in &mutations {
            let rec = match m {
                Mutation::Put {
                    partition_id,
                    key,
                    row,
                } => WalRecord::Put {
                    txn_id,
                    partition_id: *partition_id,
                    key: key.clone(),
                    row: row.clone(),
                    version: commit_version,
                },
                Mutation::Delete { partition_id, key } => WalRecord::Delete {
                    txn_id,
                    partition_id: *partition_id,
                    key: key.clone(),
                    version: commit_version,
                },
            };
            commit_guard.wal.append(&rec)?;
        }

        commit_guard.wal.append_commit(&WalRecord::Commit {
            txn_id,
            version: commit_version,
        })?;

        // 5. Apply mutations to active memtable and update visible_version
        {
            let mut read_guard = self.read_state.write();
            for m in mutations {
                match m {
                    Mutation::Put {
                        partition_id,
                        key,
                        row,
                    } => {
                        read_guard.active.apply(
                            partition_id,
                            key,
                            commit_version,
                            ValueKind::Put(row),
                        )?;
                    }
                    Mutation::Delete { partition_id, key } => {
                        read_guard.active.apply(
                            partition_id,
                            key,
                            commit_version,
                            ValueKind::Delete,
                        )?;
                    }
                }
            }
            read_guard.visible_version = commit_version;
        }

        // 6. Check memtable size for auto-flush
        let should_flush =
            self.read_state.read().active.approximate_size_bytes() >= self.options.memtable_bytes;
        if should_flush {
            self.flush_locked(&mut commit_guard)?;
        }

        Ok(commit_version)
    }

    /// Flush the active memtable to a new SST file on disk.
    pub fn flush(&self) -> Result<()> {
        let mut commit_guard = self.commit_lock.lock();
        self.flush_locked(&mut commit_guard)
    }

    /// Internal flush implementation under the commit mutex.
    ///
    /// # Crash Consistency and Ordering
    ///
    /// 1. Return immediately if active memtable is empty.
    /// 2. Detach active memtable to immutable list and install fresh active memtable.
    /// 3. Write entries to `sst/<id>.sst.tmp` via [`SstWriter::write`].
    /// 4. Rename to `sst/<id>.sst` and fsync `sst/` directory.
    /// 5. Prepend SST to manifest, atomic publish `MANIFEST`, and fsync `<dir>`.
    /// 6. Install new [`SstReader`] and drop immutable memtable.
    /// 7. Append `WalRecord::Checkpoint`, fsync WAL, and garbage collect WAL segments.
    ///
    /// Ordering rationale:
    /// - Before step 5, the WAL is authoritative and any orphan `.tmp` files are ignored on recovery.
    /// - At step 5, the SST becomes permanently durable and visible in the manifest.
    /// - Only after both SST and checkpoint are durable (step 7) is the WAL GC'd.
    /// - A crash between 5 and 7 causes recovery to replay records already in the SST,
    ///   which [`Memtable::apply`] tolerates idempotently.
    fn flush_locked(&self, commit_guard: &mut CommitState) -> Result<()> {
        // 1. Check if active memtable is empty
        {
            let read_guard = self.read_state.read();
            if read_guard.active.is_empty() {
                return Ok(());
            }
        }

        // 2. Move active into immutable list; install fresh active memtable
        let old_active = {
            let mut read_guard = self.read_state.write();
            let old = std::mem::take(&mut read_guard.active);
            let old = Arc::new(old);
            read_guard.immutables.insert(0, Arc::clone(&old));
            old
        };

        // 3. Write detached entries to sst/<id>.sst.tmp
        let sst_id = commit_guard.next_sst_id;
        commit_guard.next_sst_id += 1;

        let sst_dir = self.options.dir.join("sst");
        let tmp_path = sst_dir.join(format!("{sst_id}.sst.tmp"));
        let sst_path = sst_dir.join(format!("{sst_id}.sst"));

        let meta = SstWriter::write(
            &tmp_path,
            sst_id,
            old_active.iter().cloned(),
            &self.options.sst,
        )?;

        // 4. Rename to sst/<id>.sst and fsync sst/ directory
        std::fs::rename(&tmp_path, &sst_path)?;
        sync_dir(&sst_dir)?;

        // 5. Write and fsync MANIFEST
        let mut new_manifest = commit_guard.manifest.clone();
        new_manifest.prepend(ManifestSstEntry::from(&meta));
        Manifest::atomic_publish(&self.options.dir, &new_manifest)?;
        commit_guard.manifest = new_manifest;

        // 6. Install new SstReader and drop immutable memtable
        let reader = Arc::new(SstReader::open(&sst_path)?);
        {
            let mut read_guard = self.read_state.write();
            read_guard.ssts.insert(0, reader);
            read_guard
                .immutables
                .retain(|m| !Arc::ptr_eq(m, &old_active));
        }

        // 7. Checkpoint WAL and GC superseded segments
        let flushed_max_version = meta.max_version.unwrap_or(Version::INITIAL);
        commit_guard.wal.append(&WalRecord::Checkpoint {
            version: flushed_max_version,
        })?;
        commit_guard.wal.sync()?;
        let _ = commit_guard.wal.gc(flushed_max_version)?;

        Ok(())
    }

    /// Find the newest committed version of a key across active memtable,
    /// immutable memtables, and SST readers.
    fn find_newest_version(
        read_guard: &ReadState,
        partition_id: u64,
        key: &[u8],
    ) -> Result<Option<Version>> {
        // 1. Active memtable (newest)
        if let Some(entry) = read_guard
            .active
            .get(partition_id, key, Version::new(u64::MAX))
        {
            return Ok(Some(entry.key.version));
        }

        // 2. Immutable memtables (newest first)
        for imm in &read_guard.immutables {
            if let Some(entry) = imm.get(partition_id, key, Version::new(u64::MAX)) {
                return Ok(Some(entry.key.version));
            }
        }

        // 3. SST readers (newest first)
        for sst in &read_guard.ssts {
            if let Some(entry) = sst.get(partition_id, key, Version::new(u64::MAX))? {
                return Ok(Some(entry.key.version));
            }
        }

        Ok(None)
    }
}
