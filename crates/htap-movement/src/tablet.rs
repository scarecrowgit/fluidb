//! Truthful logical snapshot clone and repair simulation for tablets.
//!
//! # Concurrency and Consistency Guarantees
//! - **Truthful Logical Snapshot Clone**: Clones tablet data by pinning a point-in-time
//!   MVCC snapshot on the rowstore [`htap_rowstore::Engine`], scanning partition entries,
//!   and collapsing MVCC versions and tombstones into deterministic logical rows.
//!   Engine-global WAL or SST files are **never** copied directly.
//! - **Durable Checksummed Package Manifest & Artifact**: Stores cloned tablet data under
//!   the movement root directory keyed by source tablet, target replica, and job ID:
//!   `<movement_root>/tablets/<source_tablet_id>/<target_replica_id>/<job_id>/`.
//!   The package consists of:
//!   - `MANIFEST`: Envelope-protected metadata including table schema, entity IDs,
//!     base pinned MVCC [`Version`], logical row count, and CRC32C checksum of the
//!     data artifact.
//!   - `DATA`: Serialized logical rows payload matching the manifest checksum.
//!
//!   Publication is atomic via temporary file write, fsync, rename, and directory fsync.
//! - **Idempotent Clone**: If `clone_tablet` is invoked with an existing `job_id`, it
//!   validates the completed package on disk and returns the existing manifest without
//!   re-executing storage scans. If the completed package is corrupt, retry rejects it.
//! - **Package Verification**: `verify_package` independently verifies envelope integrity,
//!   payload CRC32C, row count, schema against the current catalog, entity IDs, and
//!   base version constraints.
//! - **Repair Simulation**: `repair_tablet` verifies package integrity, then performs an
//!   atomic CAS update against [`htap_catalog::store::CatalogStore`] to transition the
//!   target replica from `healthy = false` to `healthy = true` while incrementing its
//!   generation and preserving cluster topology. If the package is corrupt or mismatched,
//!   repair is strictly refused and catalog state remains untouched.
//! - **Non-Goals & Single-Node Simulation Scope**: This module is a truthful local simulation.
//!   It does **not** implement remote network transports, physical distributed replica
//!   storage engines, Raft/Paxos consensus, placement drivers, leader election, or live
//!   serving replica traffic handoffs.

use std::fs::{self, File};
use std::io::Write;
use std::time::Instant;

use htap_catalog::store::CatalogStore;
use htap_catalog::{PartitionId, ReplicaDescriptor, ReplicaId, TableDescriptor, TableId, TabletId};
use htap_common::{read_file_exact_bounded, HtapError, Result, Row, Schema, Version};
use htap_rowstore::{Engine, Snapshot};
use serde::{Deserialize, Serialize};

use crate::export::collapse_entries_to_rows;
use crate::job::{
    sync_dir, validate_job_id, CopyReport, DataFormat, LocalDataMover, MovementJobKind,
    MovementJobRequest,
};

/// Fixed 8-byte header magic identifying durable tablet package manifest envelopes (`HTAPMNF1`).
pub const MANIFEST_HEADER_MAGIC: &[u8; 8] = b"HTAPMNF1";
/// Supported tablet package manifest binary format version.
pub const MANIFEST_FORMAT_VERSION: u16 = 1;
/// Fixed header length (8 magic + 2 version + 4 payload_len + 4 crc32c = 18 bytes).
pub const MANIFEST_HEADER_LEN: usize = 18;
/// Maximum allowed manifest JSON payload size (16 MiB).
pub const MAX_MANIFEST_PAYLOAD_BYTES: u32 = 16 * 1024 * 1024;
/// Maximum allowed tablet package data payload size (64 MiB).
pub const MAX_PACKAGE_DATA_BYTES: u64 = 64 * 1024 * 1024;

/// User options for cloning a source tablet snapshot to a target replica.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabletCloneOptions {
    /// Unique movement job identifier.
    pub job_id: String,
    /// Optional target table identifier (resolved from source tablet if None).
    pub table_id: Option<TableId>,
    /// Source tablet identifier to clone from.
    pub source_tablet_id: TabletId,
    /// Target replica identifier that will receive or be repaired by this clone.
    pub target_replica_id: ReplicaId,
    /// Optional pinned MVCC snapshot version. If `None`, engine snapshot is pinned at call time.
    pub pinned_version: Option<Version>,
}

impl TabletCloneOptions {
    /// Create a new clone options descriptor with default settings.
    pub fn new(
        job_id: impl Into<String>,
        source_tablet_id: TabletId,
        target_replica_id: ReplicaId,
    ) -> Self {
        Self {
            job_id: job_id.into(),
            table_id: None,
            source_tablet_id,
            target_replica_id,
            pinned_version: None,
        }
    }

    /// Set an explicit expected table ID.
    #[inline]
    pub fn with_table_id(mut self, table_id: TableId) -> Self {
        self.table_id = Some(table_id);
        self
    }

    /// Pin an explicit MVCC snapshot version.
    #[inline]
    pub fn with_pinned_version(mut self, version: Version) -> Self {
        self.pinned_version = Some(version);
        self
    }

    /// Validate options parameters.
    pub fn validate(&self) -> Result<()> {
        validate_job_id(&self.job_id)?;
        Ok(())
    }
}

/// Durable manifest describing a truthful logical tablet snapshot clone package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabletPackageManifest {
    /// Unique movement job identifier.
    pub job_id: String,
    /// Target table identifier.
    pub table_id: TableId,
    /// Target partition identifier.
    pub partition_id: PartitionId,
    /// Source tablet identifier.
    pub source_tablet_id: TabletId,
    /// Target replica identifier.
    pub target_replica_id: ReplicaId,
    /// Table schema captured at clone snapshot time.
    pub schema: Schema,
    /// Rowstore MVCC commit version pinned during clone.
    pub base_version: Version,
    /// Total count of collapsed visible logical rows in payload artifact.
    pub row_count: u64,
    /// CRC32C checksum of raw data artifact payload bytes.
    pub payload_checksum: u32,
    /// Total bytes of raw data artifact payload.
    pub payload_bytes: u64,
}

/// Encode a [`TabletPackageManifest`] into a versioned, CRC32C-checked binary envelope.
pub fn encode_manifest(manifest: &TabletPackageManifest) -> Result<Vec<u8>> {
    if manifest.payload_bytes > MAX_PACKAGE_DATA_BYTES {
        return Err(HtapError::InvalidArgument(format!(
            "manifest payload_bytes {} exceeds maximum allowed {}",
            manifest.payload_bytes, MAX_PACKAGE_DATA_BYTES
        )));
    }

    let payload = serde_json::to_vec(manifest)
        .map_err(|e| HtapError::Internal(format!("failed to serialize tablet manifest: {e}")))?;

    if payload.len() > MAX_MANIFEST_PAYLOAD_BYTES as usize {
        return Err(HtapError::InvalidArgument(format!(
            "manifest payload size {} exceeds maximum limit {}",
            payload.len(),
            MAX_MANIFEST_PAYLOAD_BYTES
        )));
    }

    let payload_len = payload.len() as u32;
    let checksum = crc32c::crc32c(&payload);

    let mut buf = Vec::with_capacity(MANIFEST_HEADER_LEN + payload.len());
    buf.extend_from_slice(MANIFEST_HEADER_MAGIC);
    buf.extend_from_slice(&MANIFEST_FORMAT_VERSION.to_le_bytes());
    buf.extend_from_slice(&payload_len.to_le_bytes());
    buf.extend_from_slice(&checksum.to_le_bytes());
    buf.extend_from_slice(&payload);

    Ok(buf)
}

/// Decode and validate a [`TabletPackageManifest`] from a versioned binary envelope.
pub fn decode_manifest(bytes: &[u8]) -> Result<TabletPackageManifest> {
    if bytes.len() < MANIFEST_HEADER_LEN {
        return Err(HtapError::Corruption(format!(
            "manifest file too small: {} bytes, minimum header length is {}",
            bytes.len(),
            MANIFEST_HEADER_LEN
        )));
    }

    if &bytes[0..8] != MANIFEST_HEADER_MAGIC {
        return Err(HtapError::Corruption(
            "invalid tablet package manifest header magic bytes".into(),
        ));
    }

    let version = u16::from_le_bytes(bytes[8..10].try_into().unwrap());
    if version != MANIFEST_FORMAT_VERSION {
        return Err(HtapError::Corruption(format!(
            "unsupported tablet package manifest format version: {version}"
        )));
    }

    let payload_len = u32::from_le_bytes(bytes[10..14].try_into().unwrap());
    let expected_crc = u32::from_le_bytes(bytes[14..18].try_into().unwrap());

    if payload_len > MAX_MANIFEST_PAYLOAD_BYTES {
        return Err(HtapError::Corruption(format!(
            "manifest payload length {} exceeds maximum limit {}",
            payload_len, MAX_MANIFEST_PAYLOAD_BYTES
        )));
    }

    let expected_total = MANIFEST_HEADER_LEN + payload_len as usize;
    if bytes.len() < expected_total {
        return Err(HtapError::Corruption(format!(
            "truncated manifest file: expected {} bytes, found {}",
            expected_total,
            bytes.len()
        )));
    }

    if bytes.len() > expected_total {
        return Err(HtapError::Corruption(format!(
            "manifest file has {} trailing leftover bytes",
            bytes.len() - expected_total
        )));
    }

    let payload = &bytes[MANIFEST_HEADER_LEN..expected_total];
    let computed_crc = crc32c::crc32c(payload);
    if computed_crc != expected_crc {
        return Err(HtapError::Corruption(format!(
            "manifest checksum mismatch: expected {expected_crc:#010x}, got {computed_crc:#010x}"
        )));
    }

    let manifest: TabletPackageManifest = serde_json::from_slice(payload)
        .map_err(|e| HtapError::Corruption(format!("failed to deserialize manifest JSON: {e}")))?;

    if manifest.payload_bytes > MAX_PACKAGE_DATA_BYTES {
        return Err(HtapError::Corruption(format!(
            "manifest payload_bytes {} exceeds maximum limit {}",
            manifest.payload_bytes, MAX_PACKAGE_DATA_BYTES
        )));
    }

    Ok(manifest)
}

/// Resolved local topology context for tablet clone/repair operations.
#[derive(Debug, Clone)]
struct ResolvedTabletTopology {
    table: TableDescriptor,
    partition_id: PartitionId,
    source_tablet_id: TabletId,
    _target_replica: ReplicaDescriptor,
}

/// Resolve and strictly validate single-tablet local topology from CatalogStore.
///
/// Ensures:
/// 1. Catalog is non-empty.
/// 2. Source tablet exists.
/// 3. Tablet partition exists.
/// 4. Table exists, and matches `options.table_id` if provided.
/// 5. Table has exactly one partition.
/// 6. Partition has exactly one tablet (matching `source_tablet_id`).
/// 7. Tablet has exactly one healthy leader replica.
/// 8. Target replica exists and belongs to this tablet.
/// 9. Table has a valid schema with primary key.
fn resolve_clone_topology(
    catalog: &dyn CatalogStore,
    options: &TabletCloneOptions,
) -> Result<ResolvedTabletTopology> {
    let snapshot = catalog
        .load()?
        .ok_or_else(|| HtapError::NotFound("catalog is empty".into()))?;

    let tablet = snapshot
        .tablet(options.source_tablet_id)
        .ok_or_else(|| {
            HtapError::NotFound(format!(
                "source tablet {} not found",
                options.source_tablet_id
            ))
        })?
        .clone();

    let partition = snapshot
        .partition(tablet.partition_id)
        .ok_or_else(|| {
            HtapError::NotFound(format!(
                "partition {} for tablet {} not found",
                tablet.partition_id, tablet.id
            ))
        })?
        .clone();

    let table = snapshot
        .table(partition.table_id)
        .ok_or_else(|| {
            HtapError::NotFound(format!(
                "table {} for partition {} not found",
                partition.table_id, partition.id
            ))
        })?
        .clone();

    if let Some(expected_table_id) = options.table_id {
        if table.id != expected_table_id {
            return Err(HtapError::InvalidArgument(format!(
                "requested table ID {} does not match source tablet table ID {}",
                expected_table_id, table.id
            )));
        }
    }

    // Require local one-tablet topology
    let partitions = snapshot.table_partitions(table.id);
    if partitions.len() != 1 {
        return Err(HtapError::InvalidArgument(format!(
            "table {} must have exactly 1 partition for local data movement, found {}",
            table.id,
            partitions.len()
        )));
    }

    let tablets = snapshot.partition_tablets(partition.id);
    if tablets.len() != 1 {
        return Err(HtapError::InvalidArgument(format!(
            "partition {} must have exactly 1 tablet for local data movement, found {}",
            partition.id,
            tablets.len()
        )));
    }
    if tablets[0].id != options.source_tablet_id {
        return Err(HtapError::InvalidArgument(format!(
            "partition tablet ID {} does not match requested source tablet ID {}",
            tablets[0].id, options.source_tablet_id
        )));
    }

    // Leader replica check: source tablet must have exactly 1 healthy leader replica
    let replicas = snapshot.tablet_replicas(tablet.id);
    let healthy_leaders: Vec<_> = replicas
        .iter()
        .filter(|r| r.is_leader && r.healthy)
        .collect();
    if healthy_leaders.len() != 1 {
        return Err(HtapError::Internal(format!(
            "tablet {} must have exactly 1 healthy leader replica, found {}",
            tablet.id,
            healthy_leaders.len()
        )));
    }

    // Target replica check
    let target_replica = snapshot
        .replica(options.target_replica_id)
        .ok_or_else(|| {
            HtapError::NotFound(format!(
                "target replica {} not found in catalog",
                options.target_replica_id
            ))
        })?
        .clone();

    if target_replica.tablet_id != tablet.id {
        return Err(HtapError::InvalidArgument(format!(
            "target replica {} belongs to tablet {}, not source tablet {}",
            target_replica.id, target_replica.tablet_id, tablet.id
        )));
    }

    // Primary key check
    if table.primary_key.is_empty() && table.schema.primary_key_indices().is_empty() {
        return Err(HtapError::InvalidArgument(format!(
            "table {} has no primary key columns defined",
            table.id
        )));
    }

    Ok(ResolvedTabletTopology {
        table,
        partition_id: partition.id,
        source_tablet_id: tablet.id,
        _target_replica: target_replica,
    })
}

/// Clone a source tablet partition snapshot into a durable logical package.
///
/// # Concurrency and Consistency
/// - Resolves and enforces local one-tablet topology from `catalog`.
/// - Pins a rowstore snapshot (using `options.pinned_version` or `engine.snapshot()`).
/// - Scans partition records and collapses MVCC versions and tombstones into logical rows.
/// - Writes a durable, CRC32C-checked `DATA` payload artifact and `MANIFEST` envelope under
///   `<movement_root>/tablets/<source_tablet_id>/<target_replica_id>/<job_id>/`.
/// - Employs atomic two-phase write: temporary file write, fsync, atomic rename, and directory fsync.
/// - Semantically idempotent: If invoked again with the same `job_id`, it validates the
///   completed package on disk and returns the existing manifest without re-executing storage scans.
pub fn clone_tablet(
    mover: &LocalDataMover,
    options: &TabletCloneOptions,
    catalog: &dyn CatalogStore,
    engine: &Engine,
) -> Result<TabletPackageManifest> {
    options.validate()?;
    let start_instant = Instant::now();

    let manifest_path = mover.tablet_manifest_path(
        options.source_tablet_id,
        options.target_replica_id,
        &options.job_id,
    )?;

    // Idempotent retry: if package manifest already exists, validate completed package
    if manifest_path.exists() {
        return verify_package(mover, options, catalog);
    }

    // Resolve topology and enforce local one-tablet topology
    let topology = resolve_clone_topology(catalog, options)?;

    // Register job in LocalDataMover to track lifecycle
    let job_request = MovementJobRequest {
        job_id: options.job_id.clone(),
        kind: MovementJobKind::Clone,
        table_id: topology.table.id,
        tablet_id: topology.source_tablet_id,
        format: DataFormat::JsonLines,
        path: manifest_path.clone(),
        batch_rows: 1_000,
        max_errors: 0,
        delimiter: b',',
        has_header: true,
        pinned_version: options.pinned_version,
    };
    let job = mover.start_job(job_request)?;
    if job.is_complete() {
        return verify_package(mover, options, catalog);
    }
    if job.is_failed() {
        return Err(HtapError::Conflict(format!(
            "job '{}' previously failed: {}",
            job.job_id(),
            job.error.as_deref().unwrap_or("unknown error")
        )));
    }

    // Pin MVCC snapshot once
    let snapshot = options
        .pinned_version
        .map(Snapshot::new)
        .unwrap_or_else(|| engine.snapshot());
    let base_version = snapshot.version;

    // Scan partition and collapse MVCC versions / tombstones into logical visible rows
    let entries = engine.scan_partition(topology.partition_id.as_u64(), snapshot)?;
    let rows = collapse_entries_to_rows(&entries);
    let row_count = rows.len() as u64;

    // Serialize rows data artifact
    let data_bytes = serde_json::to_vec(&rows)
        .map_err(|e| HtapError::Internal(format!("failed to serialize tablet rows: {e}")))?;
    let payload_checksum = crc32c::crc32c(&data_bytes);
    let payload_bytes = data_bytes.len() as u64;

    let manifest = TabletPackageManifest {
        job_id: options.job_id.clone(),
        table_id: topology.table.id,
        partition_id: topology.partition_id,
        source_tablet_id: topology.source_tablet_id,
        target_replica_id: options.target_replica_id,
        schema: topology.table.schema.clone(),
        base_version,
        row_count,
        payload_checksum,
        payload_bytes,
    };
    let manifest_bytes = encode_manifest(&manifest)?;

    // Atomic publication under package directory
    let package_dir = mover.tablet_package_dir(
        options.source_tablet_id,
        options.target_replica_id,
        &options.job_id,
    )?;
    fs::create_dir_all(&package_dir)?;

    let data_tmp_path = package_dir.join("DATA.tmp");
    let data_path = mover.tablet_data_path(
        options.source_tablet_id,
        options.target_replica_id,
        &options.job_id,
    )?;
    let manifest_tmp_path = package_dir.join("MANIFEST.tmp");

    // 1. Write DATA.tmp and fsync
    let write_data_res = (|| -> Result<()> {
        let mut f = File::create(&data_tmp_path)?;
        f.write_all(&data_bytes)?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(e) = write_data_res {
        let _ = fs::remove_file(&data_tmp_path);
        let _ = mover.fail_job(&options.job_id, e.to_string());
        return Err(e);
    }

    // 2. Rename DATA.tmp -> DATA
    if let Err(e) = fs::rename(&data_tmp_path, &data_path) {
        let _ = fs::remove_file(&data_tmp_path);
        let _ = mover.fail_job(&options.job_id, e.to_string());
        return Err(HtapError::Io(e));
    }

    // 3. Write MANIFEST.tmp and fsync
    let write_manifest_res = (|| -> Result<()> {
        let mut f = File::create(&manifest_tmp_path)?;
        f.write_all(&manifest_bytes)?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(e) = write_manifest_res {
        let _ = fs::remove_file(&manifest_tmp_path);
        let _ = mover.fail_job(&options.job_id, e.to_string());
        return Err(e);
    }

    // 4. Rename MANIFEST.tmp -> MANIFEST
    if let Err(e) = fs::rename(&manifest_tmp_path, &manifest_path) {
        let _ = fs::remove_file(&manifest_tmp_path);
        let _ = mover.fail_job(&options.job_id, e.to_string());
        return Err(HtapError::Io(e));
    }

    // 5. Fsync directories
    sync_dir(&package_dir)?;
    if let Some(parent) = package_dir.parent() {
        let _ = sync_dir(parent);
    }
    let _ = sync_dir(&mover.tablets_dir());
    let _ = sync_dir(mover.root_dir());

    // 6. Complete job in LocalDataMover
    let report = CopyReport {
        job_id: options.job_id.clone(),
        records_read: row_count,
        records_committed: row_count,
        rows_written: row_count,
        records_skipped: 0,
        duration_ms: start_instant.elapsed().as_millis() as u64,
    };
    mover.complete_job(&options.job_id, report)?;

    Ok(manifest)
}

/// Verify the integrity and consistency of a tablet clone package.
///
/// Checks:
/// 1. `MANIFEST` and `DATA` files exist.
/// 2. `MANIFEST` envelope header magic, format version, and payload CRC32C checksum.
/// 3. `DATA` artifact size matches `manifest.payload_bytes`.
/// 4. `DATA` artifact CRC32C matches `manifest.payload_checksum`.
/// 5. `DATA` rows deserialize successfully and row count matches `manifest.row_count`.
/// 6. Each row field count matches `manifest.schema.len()`.
/// 7. Entity IDs (`job_id`, `source_tablet_id`, `target_replica_id`, `table_id`) match options.
/// 8. Base version matches `options.pinned_version` if provided, and is strictly > 0.
/// 9. Manifest schema and IDs match the current catalog table and partition definitions.
pub fn verify_package(
    mover: &LocalDataMover,
    options: &TabletCloneOptions,
    catalog: &dyn CatalogStore,
) -> Result<TabletPackageManifest> {
    options.validate()?;

    let manifest_path = mover.tablet_manifest_path(
        options.source_tablet_id,
        options.target_replica_id,
        &options.job_id,
    )?;
    let data_path = mover.tablet_data_path(
        options.source_tablet_id,
        options.target_replica_id,
        &options.job_id,
    )?;

    // 1. Read and decode manifest envelope
    let manifest_max_bytes = MANIFEST_HEADER_LEN + MAX_MANIFEST_PAYLOAD_BYTES as usize;
    let manifest_bytes = match read_file_exact_bounded(&manifest_path, manifest_max_bytes) {
        Ok(b) => b,
        Err(HtapError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(HtapError::NotFound(format!(
                "manifest file not found at {}",
                manifest_path.display()
            )));
        }
        Err(e) => return Err(e),
    };
    let manifest = decode_manifest(&manifest_bytes)?;

    // 2. ID checks
    if manifest.job_id != options.job_id {
        return Err(HtapError::Corruption(format!(
            "job ID mismatch: manifest contains '{}', expected '{}'",
            manifest.job_id, options.job_id
        )));
    }
    if manifest.source_tablet_id != options.source_tablet_id {
        return Err(HtapError::Corruption(format!(
            "source tablet ID mismatch: manifest contains {}, expected {}",
            manifest.source_tablet_id, options.source_tablet_id
        )));
    }
    if manifest.target_replica_id != options.target_replica_id {
        return Err(HtapError::Corruption(format!(
            "target replica ID mismatch: manifest contains {}, expected {}",
            manifest.target_replica_id, options.target_replica_id
        )));
    }
    if let Some(expected_table_id) = options.table_id {
        if manifest.table_id != expected_table_id {
            return Err(HtapError::Corruption(format!(
                "table ID mismatch: manifest contains {}, expected {}",
                manifest.table_id, expected_table_id
            )));
        }
    }

    // 3. Base version checks
    if manifest.base_version.get() == 0 {
        return Err(HtapError::Corruption(
            "manifest base version must be greater than 0".into(),
        ));
    }
    if let Some(pinned) = options.pinned_version {
        if manifest.base_version != pinned {
            return Err(HtapError::Corruption(format!(
                "base version mismatch: manifest contains {}, expected pinned {}",
                manifest.base_version, pinned
            )));
        }
    }

    // 4. Schema and catalog checks
    let snapshot = catalog
        .load()?
        .ok_or_else(|| HtapError::NotFound("catalog is empty".into()))?;

    let table = snapshot.table(manifest.table_id).ok_or_else(|| {
        HtapError::NotFound(format!("table {} not found in catalog", manifest.table_id))
    })?;

    if manifest.schema != table.schema {
        return Err(HtapError::Corruption(format!(
            "schema mismatch: manifest schema does not match catalog schema for table '{}'",
            table.name
        )));
    }

    let partition = snapshot.partition(manifest.partition_id).ok_or_else(|| {
        HtapError::NotFound(format!(
            "partition {} not found in catalog",
            manifest.partition_id
        ))
    })?;
    if partition.table_id != table.id {
        return Err(HtapError::Corruption(format!(
            "partition {} belongs to table {}, expected {}",
            partition.id, partition.table_id, table.id
        )));
    }

    let tablet = snapshot.tablet(manifest.source_tablet_id).ok_or_else(|| {
        HtapError::NotFound(format!(
            "source tablet {} not found in catalog",
            manifest.source_tablet_id
        ))
    })?;
    if tablet.partition_id != partition.id {
        return Err(HtapError::Corruption(format!(
            "source tablet {} belongs to partition {}, expected {}",
            tablet.id, tablet.partition_id, partition.id
        )));
    }

    let target_replica = snapshot
        .replica(manifest.target_replica_id)
        .ok_or_else(|| {
            HtapError::NotFound(format!(
                "target replica {} not found in catalog",
                manifest.target_replica_id
            ))
        })?;
    if target_replica.tablet_id != tablet.id {
        return Err(HtapError::Corruption(format!(
            "target replica {} belongs to tablet {}, expected {}",
            target_replica.id, target_replica.tablet_id, tablet.id
        )));
    }

    // 5. Data artifact checks
    if manifest.payload_bytes > MAX_PACKAGE_DATA_BYTES {
        return Err(HtapError::Corruption(format!(
            "payload byte size {} exceeds maximum allowed {}",
            manifest.payload_bytes, MAX_PACKAGE_DATA_BYTES
        )));
    }
    let data_max_bytes = usize::try_from(manifest.payload_bytes)
        .map_err(|_| HtapError::Corruption("payload bytes exceed memory address space".into()))?;
    let data_bytes = match read_file_exact_bounded(&data_path, data_max_bytes) {
        Ok(b) => b,
        Err(HtapError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(HtapError::NotFound(format!(
                "data artifact file not found at {}",
                data_path.display()
            )));
        }
        Err(e) => return Err(e),
    };
    if data_bytes.len() as u64 != manifest.payload_bytes {
        return Err(HtapError::Corruption(format!(
            "payload byte size mismatch: manifest says {}, found {}",
            manifest.payload_bytes,
            data_bytes.len()
        )));
    }

    let computed_crc = crc32c::crc32c(&data_bytes);
    if computed_crc != manifest.payload_checksum {
        return Err(HtapError::Corruption(format!(
            "payload checksum mismatch: manifest expected {:#010x}, computed {:#010x}",
            manifest.payload_checksum, computed_crc
        )));
    }

    let rows: Vec<Row> = serde_json::from_slice(&data_bytes).map_err(|e| {
        HtapError::Corruption(format!(
            "corrupt data payload: failed to deserialize rows: {e}"
        ))
    })?;

    if rows.len() as u64 != manifest.row_count {
        return Err(HtapError::Corruption(format!(
            "row count mismatch: manifest says {}, actual decoded rows {}",
            manifest.row_count,
            rows.len()
        )));
    }

    for row in &rows {
        if row.len() != manifest.schema.len() {
            return Err(HtapError::Corruption(format!(
                "row field count {} does not match manifest schema length {}",
                row.len(),
                manifest.schema.len()
            )));
        }
    }

    Ok(manifest)
}

/// Reconcile and repair an unhealthy replica by validating the clone package and CAS-updating its health status.
///
/// # Semantics and Guarantees
/// - First validates the durable package via [`verify_package`]. If the package is corrupt,
///   truncated, or mismatched in ID/schema/base/row-count, health restoration is refused
///   and catalog state is **never** modified.
/// - Performs an atomic compare-and-set update against [`CatalogStore`]:
///   - Updates only target [`ReplicaDescriptor::healthy`] to `true`.
///   - Increments target [`ReplicaDescriptor::generation`] by 1.
///   - Increments cluster [`htap_catalog::CatalogSnapshot::generation`] by 1.
///   - Preserves all tables, partitions, tablets, and other replica descriptors unchanged.
/// - Idempotent retry: If the target replica is already healthy (e.g., following recovery
///   or repeated execution), returns the current healthy descriptor as an idempotent no-op.
pub fn repair_tablet(
    mover: &LocalDataMover,
    options: &TabletCloneOptions,
    catalog: &dyn CatalogStore,
) -> Result<ReplicaDescriptor> {
    // 1. Strictly validate package first; refuse repair if corrupt or mismatched
    let manifest = verify_package(mover, options, catalog)?;

    // 2. CAS loop to update replica health
    let mut attempts = 0;
    loop {
        attempts += 1;
        let snapshot = catalog
            .load()?
            .ok_or_else(|| HtapError::NotFound("catalog is empty".into()))?;

        let target_rep = snapshot
            .replica(manifest.target_replica_id)
            .ok_or_else(|| {
                HtapError::NotFound(format!(
                    "target replica {} not found in catalog",
                    manifest.target_replica_id
                ))
            })?;

        if target_rep.tablet_id != manifest.source_tablet_id {
            return Err(HtapError::InvalidArgument(format!(
                "target replica {} belongs to tablet {}, expected source tablet {}",
                target_rep.id, target_rep.tablet_id, manifest.source_tablet_id
            )));
        }

        // Reopen / idempotent retry: if already healthy, return current descriptor
        if target_rep.healthy {
            return Ok(target_rep.clone());
        }

        // Build next snapshot preserving topology exactly
        let mut next_snapshot = snapshot.clone();
        let mut updated_replica = None;
        for rep in &mut next_snapshot.replicas {
            if rep.id == manifest.target_replica_id {
                rep.healthy = true;
                rep.generation =
                    rep.generation
                        .checked_add(1)
                        .ok_or(HtapError::CounterOverflow {
                            counter: "replica_generation",
                        })?;
                updated_replica = Some(rep.clone());
                break;
            }
        }
        let updated = updated_replica.expect("target replica must exist");
        next_snapshot.generation =
            snapshot
                .generation
                .checked_add(1)
                .ok_or(HtapError::CounterOverflow {
                    counter: "catalog_generation",
                })?;

        match catalog.compare_and_set(snapshot.generation, next_snapshot) {
            Ok(()) => return Ok(updated),
            Err(HtapError::Conflict(_)) if attempts < 5 => continue,
            Err(e) => return Err(e),
        }
    }
}
