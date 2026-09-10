//! Row<->column conversion engine and durable tablet columnar manifests.
//!
//! Provides the crash-safe [`TabletColumnManifest`], writer helpers for columnar segments,
//! and envelope serialization with CRC32-C verification.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use htap_catalog::store::CatalogStore;
use htap_catalog::{
    ColumnManifestRef, ConversionDescriptor, ConversionPhase, PartitionId, StorageDescriptor,
    StorageFormat, TabletId,
};
pub use htap_catalog::{MAX_MANIFEST_ROWS, MAX_MANIFEST_SEGMENTS};
use htap_colstore::{
    validate_segment_schema, ScanRequest, SegmentOptions, SegmentReader, SegmentWriter,
};
use htap_common::{
    encode_key, read_file_exact_bounded, HtapError, Result, Row, Schema, Value, Version,
};
use htap_rowstore::{Engine, MemtableEntry, Snapshot, ValueKind};
use serde::{Deserialize, Serialize};

/// Header magic bytes for tablet manifest files (`HTAPTBM1`).
pub const HEADER_MAGIC: &[u8; 8] = b"HTAPTBM1";

/// Supported tablet manifest binary envelope format version.
pub const FORMAT_VERSION: u16 = 1;

/// Fixed envelope header size (8 magic + 2 format + 4 payload_len + 4 crc32c = 18 bytes).
pub const HEADER_LEN: usize = 18;

/// Upper bound on manifest payload size (64 MiB) to guard against unbounded allocations.
pub const MAX_MANIFEST_PAYLOAD_BYTES: u32 = 64 * 1024 * 1024;

/// Canonical manifest file name on disk.
pub const MANIFEST_FILE_NAME: &str = "MANIFEST";

/// Temporary file name used for atomic two-phase publication.
pub const MANIFEST_TMP_FILE_NAME: &str = "MANIFEST.tmp";

/// Optional summary metadata for a columnar segment entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SegmentSummary {
    /// Number of row-aligned blocks in the columnar segment.
    pub block_count: usize,
    /// Total byte size of the segment file on disk.
    pub file_bytes: u64,
    /// Number of columns in the segment schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column_count: Option<usize>,
}

impl SegmentSummary {
    /// Create a new segment summary with block count and file byte size.
    pub fn new(block_count: usize, file_bytes: u64) -> Self {
        Self {
            block_count,
            file_bytes,
            column_count: None,
        }
    }

    /// Set the optional column count on this summary.
    pub fn with_column_count(mut self, column_count: usize) -> Self {
        self.column_count = Some(column_count);
        self
    }
}

/// An entry representing a durable columnar segment registered within a tablet manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentEntry {
    /// Relative path to the segment file (relative to tablet directory or manifest root).
    pub path: String,
    /// Total number of rows contained in this segment.
    pub row_count: u64,
    /// Optional summary metadata describing segment layout and size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<SegmentSummary>,
}

impl SegmentEntry {
    /// Create a new segment entry with path, row count, and optional summary.
    pub fn new(path: impl Into<String>, row_count: u64, summary: Option<SegmentSummary>) -> Self {
        Self {
            path: path.into(),
            row_count,
            summary,
        }
    }
}

/// Validate that a segment path is relative, non-empty, and free of parent traversal components.
pub fn validate_segment_path(path_str: &str) -> Result<()> {
    let trimmed = path_str.trim();
    if trimmed.is_empty() {
        return Err(HtapError::InvalidArgument(
            "segment path cannot be empty".into(),
        ));
    }
    if path_str.starts_with('/') || path_str.starts_with('\\') {
        return Err(HtapError::InvalidArgument(format!(
            "segment path '{path_str}' must be relative, not absolute"
        )));
    }
    let p = Path::new(path_str);
    if p.is_absolute() {
        return Err(HtapError::InvalidArgument(format!(
            "segment path '{path_str}' must be relative, not absolute"
        )));
    }
    for comp in p.components() {
        match comp {
            std::path::Component::ParentDir => {
                return Err(HtapError::InvalidArgument(format!(
                    "segment path '{path_str}' cannot contain parent directory traversal ('..')"
                )));
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err(HtapError::InvalidArgument(format!(
                    "segment path '{path_str}' must be relative, not absolute"
                )));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Fsync a directory to ensure metadata operations like renames are durable.
pub fn sync_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let f = std::fs::File::open(path)?;
        f.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// Resolves the tablet-specific directory inside `root_dir`.
pub fn tablet_dir(root_dir: &Path, tablet_id: TabletId) -> PathBuf {
    let id_str = tablet_id.as_u64().to_string();
    let name_hyphen = format!("tablet-{id_str}");
    let name_underscore = format!("tablet_{id_str}");
    if let Some(name) = root_dir.file_name().and_then(|n| n.to_str()) {
        if name == name_hyphen || name == name_underscore || name == id_str {
            return root_dir.to_path_buf();
        }
    }
    if root_dir.join(&name_underscore).is_dir() {
        return root_dir.join(name_underscore);
    }
    root_dir.join(name_hyphen)
}

/// Computes the path to the `MANIFEST` file for the given `tablet_id` under `root_dir`.
pub fn manifest_path(root_dir: &Path, tablet_id: TabletId) -> PathBuf {
    if root_dir.join(MANIFEST_FILE_NAME).is_file() {
        if let Some(name) = root_dir.file_name().and_then(|n| n.to_str()) {
            let id_str = tablet_id.as_u64().to_string();
            let name_hyphen = format!("tablet-{id_str}");
            let name_underscore = format!("tablet_{id_str}");
            if name == name_hyphen || name == name_underscore || name == id_str {
                return root_dir.join(MANIFEST_FILE_NAME);
            }
        }
    }
    let t_dir = tablet_dir(root_dir, tablet_id);
    t_dir.join(MANIFEST_FILE_NAME)
}

/// Resolves an on-disk filesystem path for a segment entry.
pub fn resolve_segment_path(
    root_dir: &Path,
    tablet_id: TabletId,
    rel_path: &str,
) -> Result<PathBuf> {
    validate_segment_path(rel_path)?;
    let p = Path::new(rel_path);
    for comp in p.components() {
        match comp {
            std::path::Component::Normal(os_str) => {
                let s = os_str.to_str().ok_or_else(|| {
                    HtapError::InvalidArgument(format!(
                        "segment path '{rel_path}' contains invalid UTF-8"
                    ))
                })?;
                if s.contains('\0') || s == "." || s == ".." || s.contains("..") {
                    return Err(HtapError::InvalidArgument(format!(
                        "segment path '{rel_path}' contains forbidden component '{s}'"
                    )));
                }
            }
            _ => {
                return Err(HtapError::InvalidArgument(format!(
                    "segment path '{rel_path}' contains non-normal component"
                )));
            }
        }
    }
    let t_dir = tablet_dir(root_dir, tablet_id);
    let tab_candidate = t_dir.join(rel_path);
    if tab_candidate.exists() {
        return Ok(tab_candidate);
    }
    let root_candidate = root_dir.join(rel_path);
    if root_candidate.exists() {
        return Ok(root_candidate);
    }
    Ok(tab_candidate)
}

/// Authoritative manifest of columnar segments belonging to a tablet.
///
/// Records the catalog generation, tablet ID, schema, MVCC base snapshot version,
/// and an ordered list of relative segment file references.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabletColumnManifest {
    /// Catalog metadata generation when this manifest was published.
    pub generation: u64,
    /// Identifier of the tablet described by this manifest.
    pub tablet_id: TabletId,
    /// Relational schema defining column order, names, and data types.
    pub schema: Schema,
    /// MVCC base snapshot version pinned when segments were produced.
    pub base_version: Version,
    /// Ordered list of relative columnar segment entries.
    pub segments: Vec<SegmentEntry>,
}

impl TabletColumnManifest {
    /// Create a new tablet columnar manifest.
    pub fn new(
        generation: u64,
        tablet_id: TabletId,
        schema: Schema,
        base_version: Version,
        segments: Vec<SegmentEntry>,
    ) -> Self {
        Self {
            generation,
            tablet_id,
            schema,
            base_version,
            segments,
        }
    }

    /// Calculate the total number of rows across all registered segments.
    pub fn total_rows(&self) -> u64 {
        self.segments.iter().map(|s| s.row_count).sum()
    }

    /// Return the count of registered segments.
    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    /// Validate internal integrity and constraints of this manifest.
    pub fn validate(&self) -> Result<()> {
        if self.generation == 0 {
            return Err(HtapError::InvalidArgument(
                "manifest generation must be > 0".into(),
            ));
        }
        if self.base_version.get() == 0 {
            return Err(HtapError::InvalidArgument(
                "manifest base_version must be > 0".into(),
            ));
        }

        validate_segment_schema(&self.schema)?;

        let seg_count = self.segments.len() as u64;
        if seg_count > MAX_MANIFEST_SEGMENTS {
            return Err(HtapError::InvalidArgument(format!(
                "manifest segment count {seg_count} exceeds maximum allowed {MAX_MANIFEST_SEGMENTS}"
            )));
        }

        let total_r = self.total_rows();
        if total_r > MAX_MANIFEST_ROWS {
            return Err(HtapError::InvalidArgument(format!(
                "manifest total rows {total_r} exceeds maximum allowed {MAX_MANIFEST_ROWS}"
            )));
        }

        let mut seen_paths = HashSet::with_capacity(self.segments.len());
        for entry in &self.segments {
            validate_segment_path(&entry.path)?;
            if !seen_paths.insert(&entry.path) {
                return Err(HtapError::InvalidArgument(format!(
                    "duplicate segment path in manifest: {}",
                    entry.path
                )));
            }
            if let Some(summary) = &entry.summary {
                if let Some(col_cnt) = summary.column_count {
                    if col_cnt != self.schema.len() {
                        return Err(HtapError::InvalidArgument(format!(
                            "summary column_count {col_cnt} mismatch with schema columns {}",
                            self.schema.len()
                        )));
                    }
                }
            }
        }

        Ok(())
    }

    /// Encode this manifest into versioned binary envelope bytes.
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let payload = serde_json::to_vec(self).map_err(|e| {
            HtapError::Internal(format!("failed to serialize tablet manifest JSON: {e}"))
        })?;

        if payload.len() > MAX_MANIFEST_PAYLOAD_BYTES as usize {
            return Err(HtapError::InvalidArgument(format!(
                "manifest payload size {} exceeds maximum allowed {}",
                payload.len(),
                MAX_MANIFEST_PAYLOAD_BYTES
            )));
        }

        let payload_len = payload.len() as u32;
        let checksum = crc32c::crc32c(&payload);

        let mut buf = Vec::with_capacity(HEADER_LEN + payload.len());
        buf.extend_from_slice(HEADER_MAGIC);
        buf.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        buf.extend_from_slice(&payload_len.to_le_bytes());
        buf.extend_from_slice(&checksum.to_le_bytes());
        buf.extend_from_slice(&payload);

        Ok(buf)
    }

    /// Decode and validate a manifest from binary envelope bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < HEADER_LEN {
            return Err(HtapError::Corruption(format!(
                "manifest too short: {} bytes (minimum {HEADER_LEN})",
                bytes.len()
            )));
        }

        if &bytes[0..8] != HEADER_MAGIC {
            return Err(HtapError::Corruption(
                "invalid manifest magic header".into(),
            ));
        }

        let version = u16::from_le_bytes(bytes[8..10].try_into().unwrap());
        if version != FORMAT_VERSION {
            return Err(HtapError::Corruption(format!(
                "unsupported manifest format version: {version}"
            )));
        }

        let payload_len = u32::from_le_bytes(bytes[10..14].try_into().unwrap());
        let expected_crc = u32::from_le_bytes(bytes[14..18].try_into().unwrap());

        if payload_len > MAX_MANIFEST_PAYLOAD_BYTES {
            return Err(HtapError::Corruption(format!(
                "manifest payload length {payload_len} exceeds maximum {MAX_MANIFEST_PAYLOAD_BYTES}"
            )));
        }

        let expected_total = HEADER_LEN + payload_len as usize;
        if bytes.len() < expected_total {
            return Err(HtapError::Corruption(format!(
                "truncated manifest file: expected {expected_total} bytes, found {}",
                bytes.len()
            )));
        }

        if bytes.len() > expected_total {
            return Err(HtapError::Corruption(format!(
                "manifest file has {} trailing leftover bytes",
                bytes.len() - expected_total
            )));
        }

        let payload = &bytes[HEADER_LEN..expected_total];
        let computed_crc = crc32c::crc32c(payload);
        if computed_crc != expected_crc {
            return Err(HtapError::Corruption(format!(
                "manifest checksum mismatch: expected {expected_crc:#010x}, got {computed_crc:#010x}"
            )));
        }

        let manifest: TabletColumnManifest = serde_json::from_slice(payload)
            .map_err(|e| HtapError::Corruption(format!("failed to parse manifest JSON: {e}")))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Atomically publish a manifest to disk under `root_dir`.
    pub fn write_atomic(root_dir: &Path, manifest: &TabletColumnManifest) -> Result<()> {
        write_atomic(root_dir, manifest)
    }

    /// Atomically publish this manifest to disk under `root_dir`.
    pub fn write_atomic_to(&self, root_dir: &Path) -> Result<()> {
        write_atomic(root_dir, self)
    }

    /// Open and validate a tablet manifest from disk under `root_dir`.
    pub fn open(root_dir: &Path, tablet_id: TabletId) -> Result<Self> {
        open(root_dir, tablet_id)
    }

    /// Compute the path to the manifest file for a given tablet under `root_dir`.
    pub fn manifest_path(root_dir: &Path, tablet_id: TabletId) -> PathBuf {
        manifest_path(root_dir, tablet_id)
    }

    /// Atomically write a columnar segment file, validate it, and return a [`SegmentEntry`].
    pub fn write_segment(
        root_dir: &Path,
        tablet_id: TabletId,
        generation: u64,
        segment_name: &str,
        schema: &Schema,
        rows: impl IntoIterator<Item = Row>,
        options: &SegmentOptions,
    ) -> Result<SegmentEntry> {
        write_segment(
            root_dir,
            tablet_id,
            generation,
            segment_name,
            schema,
            rows,
            options,
        )
    }

    /// Creates a [`ColumnManifestRef`] referencing this manifest for catalog registration.
    pub fn to_manifest_ref(&self, root_relative_path: impl Into<String>) -> ColumnManifestRef {
        ColumnManifestRef::new(
            self.generation,
            root_relative_path,
            self.base_version,
            self.segments.len() as u64,
            self.total_rows(),
        )
    }
}

/// Atomically publish `manifest` to disk inside `tablet/` under `root_dir`.
///
/// Follows a crash-consistent two-phase atomic publish protocol:
/// 1. Write the new manifest to `MANIFEST.tmp`.
/// 2. Fsync `MANIFEST.tmp`.
/// 3. Atomic rename of `MANIFEST.tmp` to `MANIFEST`.
/// 4. Fsync the tablet directory and root directory.
pub fn write_atomic(root_dir: &Path, manifest: &TabletColumnManifest) -> Result<()> {
    manifest.validate()?;
    let t_dir = tablet_dir(root_dir, manifest.tablet_id);
    std::fs::create_dir_all(&t_dir)?;

    let tmp_path = t_dir.join(MANIFEST_TMP_FILE_NAME);
    let final_path = t_dir.join(MANIFEST_FILE_NAME);

    let bytes = manifest.encode()?;
    {
        let mut file = File::create(&tmp_path)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }

    std::fs::rename(&tmp_path, &final_path)?;
    sync_dir(&t_dir)?;
    sync_dir(root_dir)?;
    Ok(())
}

/// Opens and validates a tablet columnar manifest from disk.
///
/// Defensively reads the manifest envelope, validates magic, version, CRC,
/// and manifest constraints. Verifies that every segment entry registered in the
/// manifest exists on disk and is readable and internally consistent via [`SegmentReader`].
///
/// Any orphan files in the tablet or generation directories that are not registered
/// in the manifest are completely ignored.
pub fn open(root_dir: &Path, tablet_id: TabletId) -> Result<TabletColumnManifest> {
    let m_path = manifest_path(root_dir, tablet_id);
    let path = if m_path.is_file() {
        m_path
    } else if root_dir.join(MANIFEST_FILE_NAME).is_file() {
        root_dir.join(MANIFEST_FILE_NAME)
    } else {
        return Err(HtapError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "manifest file not found for tablet {tablet_id} at {}",
                m_path.display()
            ),
        )));
    };

    let max_bytes = HEADER_LEN + MAX_MANIFEST_PAYLOAD_BYTES as usize;
    let bytes = read_file_exact_bounded(&path, max_bytes)?;
    let manifest = TabletColumnManifest::decode(&bytes)?;

    if manifest.tablet_id != tablet_id {
        return Err(HtapError::InvalidArgument(format!(
            "manifest tablet_id {} does not match requested tablet_id {tablet_id}",
            manifest.tablet_id
        )));
    }

    // Validate that all registered segments exist and are readable by SegmentReader.
    // Directory enumeration is intentionally avoided; only manifest-registered segments are live.
    for entry in &manifest.segments {
        let seg_path = resolve_segment_path(root_dir, tablet_id, &entry.path)?;
        let reader = SegmentReader::open(&seg_path)?;
        if reader.metadata().row_count != entry.row_count {
            return Err(HtapError::Corruption(format!(
                "segment '{}' row count mismatch: manifest records {}, segment metadata has {}",
                entry.path,
                entry.row_count,
                reader.metadata().row_count
            )));
        }
        if reader.schema() != &manifest.schema {
            return Err(HtapError::Corruption(format!(
                "segment '{}' schema mismatch with manifest schema",
                entry.path
            )));
        }
    }

    Ok(manifest)
}

/// Read and decode a manifest directly from a file path without opening registered segment files.
pub fn read_manifest_envelope(path: &Path) -> Result<TabletColumnManifest> {
    let max_bytes = HEADER_LEN + MAX_MANIFEST_PAYLOAD_BYTES as usize;
    let bytes = read_file_exact_bounded(path, max_bytes)?;
    TabletColumnManifest::decode(&bytes)
}

/// Writes a columnar segment file atomically using [`SegmentWriter`], validates it with
/// [`SegmentReader`], and returns a [`SegmentEntry`] ready to be recorded in a manifest.
///
/// # Durability and Crash Safety
/// 1. The segment is written to a temporary file (`<segment_name>.tmp`) inside `tablet/gen-<generation>`.
/// 2. `SegmentWriter` flushes and fsyncs the data before completion.
/// 3. The temporary file is fsynced to ensure durability.
/// 4. The temporary file is atomically renamed to its final target path.
/// 5. The generation directory is fsynced to persist the directory entry rename.
/// 6. [`SegmentReader`] verifies header, trailer, CRC, and blocks before returning.
pub fn write_segment(
    root_dir: &Path,
    tablet_id: TabletId,
    generation: u64,
    segment_name: &str,
    schema: &Schema,
    rows: impl IntoIterator<Item = Row>,
    options: &SegmentOptions,
) -> Result<SegmentEntry> {
    if segment_name.is_empty() {
        return Err(HtapError::InvalidArgument(
            "segment_name cannot be empty".into(),
        ));
    }
    if segment_name.contains('/')
        || segment_name.contains('\\')
        || segment_name.contains('\0')
        || segment_name == "."
        || segment_name == ".."
        || segment_name.contains("..")
    {
        return Err(HtapError::InvalidArgument(format!(
            "invalid segment_name '{segment_name}': must be a safe simple file name"
        )));
    }

    let t_dir = tablet_dir(root_dir, tablet_id);
    let gen_dir = t_dir.join(format!("gen-{generation}"));
    std::fs::create_dir_all(&gen_dir)?;

    let tmp_file_name = format!("{segment_name}.tmp");
    let tmp_path = gen_dir.join(&tmp_file_name);
    let final_path = gen_dir.join(segment_name);

    // 1. Write through SegmentWriter to tmp
    let metadata = match SegmentWriter::write(&tmp_path, schema, rows, options) {
        Ok(m) => m,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e);
        }
    };

    // 2. Explicitly fsync the written tmp file
    {
        let file = std::fs::File::open(&tmp_path)?;
        file.sync_all()?;
    }

    // 3. Rename tmp to final
    std::fs::rename(&tmp_path, &final_path)?;

    // 4. Fsync directory
    sync_dir(&gen_dir)?;
    sync_dir(&t_dir)?;

    // 5. Validate with SegmentReader before manifest publication
    let reader = SegmentReader::open(&final_path)?;
    if reader.metadata().row_count != metadata.row_count {
        return Err(HtapError::Corruption(format!(
            "segment reader row count mismatch for {}: expected {}, got {}",
            final_path.display(),
            metadata.row_count,
            reader.metadata().row_count
        )));
    }

    let file_bytes = std::fs::metadata(&final_path)?.len();
    let summary = SegmentSummary {
        block_count: reader.block_count(),
        file_bytes,
        column_count: Some(schema.len()),
    };

    let rel_path = format!("gen-{generation}/{segment_name}");
    Ok(SegmentEntry {
        path: rel_path,
        row_count: metadata.row_count,
        summary: Some(summary),
    })
}

/// Collapses raw rowstore MVCC and tombstone entries for a partition into logical visible rows.
///
/// Because [`MemtableEntry`] items from `scan_partition` are ordered by user key ascending
/// and version descending, the first entry encountered for any distinct user key represents
/// its latest visible MVCC state. If that entry is a [`ValueKind::Put`], its row is retained;
/// if it is a [`ValueKind::Delete`], the key is tombstoned and omitted. Subsequent older
/// versions for the same key are discarded.
pub fn collapse_entries_to_rows(entries: &[MemtableEntry]) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut current_user_key: Option<&[u8]> = None;
    for entry in entries {
        if current_user_key == Some(entry.key.user_key.as_slice()) {
            continue;
        }
        current_user_key = Some(entry.key.user_key.as_slice());
        match &entry.value {
            ValueKind::Put(row) => rows.push(row.clone()),
            ValueKind::Delete => {}
        }
    }
    rows
}

/// Perform row-to-column conversion for a given partition.
///
/// Coordinates the atomic cutover protocol:
/// 1. Inspects catalog state and validates partition topology (requires exactly one tablet,
///    which must have exactly one healthy leader replica).
/// 2. If partition is in `StorageDescriptor::Row`, pins snapshot version from rowstore visible
///    version (or Version 1 if empty) and atomically advances catalog to `StorageDescriptor::Converting`
///    with phase [`ConversionPhase::SnapshotPinned`]. If already in `Converting`, reuses the persisted
///    pinned snapshot version and generation.
/// 3. If in `SnapshotPinned`, scans `rowstore` for the partition at the pinned snapshot version,
///    collapses MVCC versions and tombstones into logical rows, writes columnar segment file(s),
///    and atomically writes the tablet columnar manifest envelope to disk.
/// 4. Persists conversion phase transitions via catalog CAS: reloads catalog and advances phase to
///    [`ConversionPhase::SegmentsWritten`], then [`ConversionPhase::ReadyToPublish`] with a new
///    catalog generation, preserving `StorageDescriptor::Converting` and pinned snapshot/generation.
/// 5. Executes the final publish catalog CAS cutover to `StorageDescriptor::Column` requiring phase
///    [`ConversionPhase::ReadyToPublish`], clearing conversion metadata and registering the tablet
///    column manifest reference.
pub fn convert_partition(
    catalog: &dyn CatalogStore,
    rowstore: &Engine,
    colstore_root: &Path,
    options: &SegmentOptions,
    partition_id: PartitionId,
) -> Result<TabletColumnManifest> {
    let current_cat = catalog
        .load()?
        .ok_or_else(|| HtapError::NotFound("catalog is empty".into()))?;

    let part_desc = current_cat
        .partition(partition_id)
        .ok_or_else(|| HtapError::NotFound(format!("partition {partition_id} not found")))?
        .clone();

    let table_desc = current_cat
        .table(part_desc.table_id)
        .ok_or_else(|| HtapError::NotFound(format!("table {} not found", part_desc.table_id)))?
        .clone();

    // 1. Validate topology: require exactly 1 tablet, resolve it, require exactly 1 replica,
    // resolve replica, and require healthy && is_leader.
    if part_desc.tablets.is_empty() {
        return Err(HtapError::InvalidArgument(format!(
            "partition {partition_id} has no tablets"
        )));
    }
    if part_desc.tablets.len() != 1 {
        return Err(HtapError::Unsupported(format!(
            "partition {partition_id} has {} tablets; exactly 1 tablet is required for conversion",
            part_desc.tablets.len()
        )));
    }
    let tablet_id = part_desc.tablets[0];
    let tablet_desc = current_cat.tablet(tablet_id).ok_or_else(|| {
        HtapError::Internal(format!(
            "tablet {tablet_id} referenced by partition {partition_id} not found in catalog"
        ))
    })?;

    if tablet_desc.partition_id != part_desc.id {
        return Err(HtapError::Internal(format!(
            "tablet {tablet_id} partition_id mismatch: expected {partition_id}, got {}",
            tablet_desc.partition_id
        )));
    }

    if tablet_desc.replicas.is_empty() {
        return Err(HtapError::InvalidArgument(format!(
            "tablet {tablet_id} has no replicas"
        )));
    }
    if tablet_desc.replicas.len() != 1 {
        return Err(HtapError::Unsupported(format!(
            "tablet {tablet_id} has {} replicas; exactly 1 replica is required for conversion",
            tablet_desc.replicas.len()
        )));
    }
    let replica_id = tablet_desc.replicas[0];
    let replica_desc = current_cat.replica(replica_id).ok_or_else(|| {
        HtapError::Internal(format!(
            "replica {replica_id} referenced by tablet {tablet_id} not found in catalog"
        ))
    })?;

    if replica_desc.tablet_id != tablet_id {
        return Err(HtapError::Internal(format!(
            "replica {replica_id} tablet_id mismatch: expected {tablet_id}, got {}",
            replica_desc.tablet_id
        )));
    }

    if !replica_desc.healthy {
        return Err(HtapError::Internal(format!(
            "replica {replica_id} for tablet {tablet_id} is not healthy"
        )));
    }
    if !replica_desc.is_leader {
        return Err(HtapError::Internal(format!(
            "replica {replica_id} for tablet {tablet_id} is not leader"
        )));
    }

    // Determine pinned snapshot version, conversion generation, and whether manifest already exists.
    let (snapshot_version, conv_generation, manifest_opt) = match &part_desc.storage {
        StorageDescriptor::Column => {
            if part_desc.conversion.is_some() {
                return Err(HtapError::Corruption(format!(
                    "partition {partition_id} has Column storage but conversion descriptor is present"
                )));
            }
            let manifest = open(colstore_root, tablet_id)?;
            return Ok(manifest);
        }
        StorageDescriptor::Converting {
            from,
            to,
            generation,
        } => {
            if *to != StorageFormat::Column || *from != StorageFormat::Row {
                return Err(HtapError::Unsupported(format!(
                    "partition {partition_id} converting in unsupported direction: {from:?} -> {to:?}"
                )));
            }
            let conv = part_desc.conversion.as_ref().ok_or_else(|| {
                HtapError::Corruption(format!(
                    "partition {partition_id} in Converting state without conversion descriptor"
                ))
            })?;
            if conv.generation != *generation || conv.from != *from || conv.to != *to {
                return Err(HtapError::Corruption(format!(
                    "partition {partition_id} conversion descriptor does not match Converting storage"
                )));
            }

            match conv.phase {
                ConversionPhase::SnapshotPinned => {
                    // On retry when already Converting and phase is SnapshotPinned,
                    // reuse pinned snapshot & generation, build, then perform phase updates.
                    (conv.snapshot_version, *generation, None)
                }
                ConversionPhase::SegmentsWritten | ConversionPhase::ReadyToPublish => {
                    // When phase is SegmentsWritten or ReadyToPublish and matching manifest exists,
                    // do not take a new snapshot or overwrite with a different generation.
                    let disk_manifest = open(colstore_root, tablet_id).map_err(|e| {
                        HtapError::Corruption(format!(
                            "partition {partition_id} in phase {:?} but could not open manifest: {e}",
                            conv.phase
                        ))
                    })?;
                    if disk_manifest.generation != conv.generation
                        || disk_manifest.base_version != conv.snapshot_version
                    {
                        return Err(HtapError::Corruption(format!(
                            "partition {partition_id} manifest on disk (gen {}, base_version {}) does not match conversion descriptor (gen {}, base_version {})",
                            disk_manifest.generation,
                            disk_manifest.base_version,
                            conv.generation,
                            conv.snapshot_version
                        )));
                    }
                    (conv.snapshot_version, *generation, Some(disk_manifest))
                }
            }
        }
        StorageDescriptor::Row => {
            if part_desc.conversion.is_some() {
                return Err(HtapError::InvalidArgument(format!(
                    "partition {partition_id} has Row storage but conversion descriptor is present"
                )));
            }
            let visible_v = rowstore.visible_version();
            let pin_v = if visible_v.get() == 0 {
                Version::new(1)
            } else {
                visible_v
            };

            let next_gen = current_cat.generation + 1;
            let mut next_cat = current_cat.clone();
            next_cat.generation = next_gen;

            let p_mut = next_cat
                .partitions
                .iter_mut()
                .find(|p| p.id == partition_id)
                .ok_or_else(|| {
                    HtapError::NotFound(format!("partition {partition_id} not found"))
                })?;
            p_mut.generation = next_gen;
            p_mut.storage = StorageDescriptor::Converting {
                from: StorageFormat::Row,
                to: StorageFormat::Column,
                generation: next_gen,
            };
            p_mut.conversion = Some(ConversionDescriptor::new(
                next_gen,
                StorageFormat::Row,
                StorageFormat::Column,
                pin_v,
                ConversionPhase::SnapshotPinned,
            ));

            catalog.compare_and_set(current_cat.generation, next_cat)?;
            (pin_v, next_gen, None)
        }
    };

    // Build segments and publish manifest if not already durable
    let manifest = match manifest_opt {
        Some(m) => m,
        None => {
            let entries =
                rowstore.scan_partition(partition_id.as_u64(), Snapshot::new(snapshot_version))?;
            let logical_rows = collapse_entries_to_rows(&entries);

            let mut segments = Vec::new();
            if !logical_rows.is_empty() {
                let segment_name = "seg-0.col";
                let entry = write_segment(
                    colstore_root,
                    tablet_id,
                    conv_generation,
                    segment_name,
                    &table_desc.schema,
                    logical_rows,
                    options,
                )?;
                segments.push(entry);
            }

            let m = TabletColumnManifest::new(
                conv_generation,
                tablet_id,
                table_desc.schema.clone(),
                snapshot_version,
                segments,
            );
            write_atomic(colstore_root, &m)?;
            m
        }
    };

    // Transition 1: Update phase to SegmentsWritten
    let cat1 = catalog
        .load()?
        .ok_or_else(|| HtapError::NotFound("catalog is empty".into()))?;

    let p1 = cat1
        .partition(partition_id)
        .ok_or_else(|| HtapError::NotFound(format!("partition {partition_id} not found")))?;

    if matches!(p1.storage, StorageDescriptor::Column) {
        return Ok(manifest);
    }

    if let Some(conv1) = &p1.conversion {
        if conv1.generation != conv_generation
            || conv1.from != StorageFormat::Row
            || conv1.to != StorageFormat::Column
            || conv1.snapshot_version != snapshot_version
        {
            return Err(HtapError::Conflict(format!(
                "conversion descriptor mismatch during transition to SegmentsWritten: {conv1:?}"
            )));
        }

        if conv1.phase == ConversionPhase::SnapshotPinned {
            let next_gen = cat1.generation + 1;
            let mut cat_sw = cat1.clone();
            cat_sw.generation = next_gen;

            let p_mut = cat_sw
                .partitions
                .iter_mut()
                .find(|p| p.id == partition_id)
                .ok_or_else(|| {
                    HtapError::NotFound(format!("partition {partition_id} not found"))
                })?;
            p_mut.generation = next_gen;
            p_mut.storage = StorageDescriptor::Converting {
                from: StorageFormat::Row,
                to: StorageFormat::Column,
                generation: conv_generation,
            };
            p_mut.conversion = Some(ConversionDescriptor::new(
                conv_generation,
                StorageFormat::Row,
                StorageFormat::Column,
                snapshot_version,
                ConversionPhase::SegmentsWritten,
            ));

            catalog.compare_and_set(cat1.generation, cat_sw)?;
        }
    } else {
        return Err(HtapError::Corruption(format!(
            "partition {partition_id} missing conversion descriptor during SegmentsWritten update"
        )));
    }

    // Transition 2: Update phase to ReadyToPublish
    let cat2 = catalog
        .load()?
        .ok_or_else(|| HtapError::NotFound("catalog is empty".into()))?;

    let p2 = cat2
        .partition(partition_id)
        .ok_or_else(|| HtapError::NotFound(format!("partition {partition_id} not found")))?;

    if matches!(p2.storage, StorageDescriptor::Column) {
        return Ok(manifest);
    }

    if let Some(conv2) = &p2.conversion {
        if conv2.generation != conv_generation
            || conv2.from != StorageFormat::Row
            || conv2.to != StorageFormat::Column
            || conv2.snapshot_version != snapshot_version
        {
            return Err(HtapError::Conflict(format!(
                "conversion descriptor mismatch during transition to ReadyToPublish: {conv2:?}"
            )));
        }

        if conv2.phase == ConversionPhase::SegmentsWritten {
            let next_gen = cat2.generation + 1;
            let mut cat_rtp = cat2.clone();
            cat_rtp.generation = next_gen;

            let p_mut = cat_rtp
                .partitions
                .iter_mut()
                .find(|p| p.id == partition_id)
                .ok_or_else(|| {
                    HtapError::NotFound(format!("partition {partition_id} not found"))
                })?;
            p_mut.generation = next_gen;
            p_mut.storage = StorageDescriptor::Converting {
                from: StorageFormat::Row,
                to: StorageFormat::Column,
                generation: conv_generation,
            };
            p_mut.conversion = Some(ConversionDescriptor::new(
                conv_generation,
                StorageFormat::Row,
                StorageFormat::Column,
                snapshot_version,
                ConversionPhase::ReadyToPublish,
            ));

            catalog.compare_and_set(cat2.generation, cat_rtp)?;
        }
    } else {
        return Err(HtapError::Corruption(format!(
            "partition {partition_id} missing conversion descriptor during ReadyToPublish update"
        )));
    }

    // Transition 3: Final publish CAS to Column format and register manifest ref
    let cat3 = catalog
        .load()?
        .ok_or_else(|| HtapError::NotFound("catalog is empty".into()))?;

    let p3 = cat3
        .partition(partition_id)
        .ok_or_else(|| HtapError::NotFound(format!("partition {partition_id} not found")))?;

    if matches!(p3.storage, StorageDescriptor::Column) {
        return Ok(manifest);
    }

    match &p3.storage {
        StorageDescriptor::Converting {
            from,
            to,
            generation,
        } => {
            if *from != StorageFormat::Row
                || *to != StorageFormat::Column
                || *generation != conv_generation
            {
                return Err(HtapError::Conflict(format!(
                    "final CAS failed: partition storage mismatch (expected Row->Column gen {conv_generation}, found {from:?}->{to:?} gen {generation})"
                )));
            }
        }
        other => {
            return Err(HtapError::Conflict(format!(
                "final CAS failed: partition storage is {other:?}, expected Converting"
            )));
        }
    }

    let conv3 = p3.conversion.as_ref().ok_or_else(|| {
        HtapError::Conflict(format!(
            "final CAS failed: partition {partition_id} missing conversion descriptor"
        ))
    })?;

    if conv3.generation != conv_generation
        || conv3.from != StorageFormat::Row
        || conv3.to != StorageFormat::Column
        || conv3.snapshot_version != snapshot_version
        || conv3.phase != ConversionPhase::ReadyToPublish
    {
        return Err(HtapError::Conflict(format!(
            "final CAS failed: persisted conversion descriptor does not match ReadyToPublish state: {conv3:?}"
        )));
    }

    let rel_path = format!("tablet-{}/{}", tablet_id.as_u64(), MANIFEST_FILE_NAME);
    let manifest_ref = manifest.to_manifest_ref(rel_path);

    let final_gen = cat3.generation + 1;
    let mut final_cat = cat3.clone();
    final_cat.generation = final_gen;

    let p_mut = final_cat
        .partitions
        .iter_mut()
        .find(|p| p.id == partition_id)
        .ok_or_else(|| HtapError::NotFound(format!("partition {partition_id} not found")))?;
    p_mut.generation = final_gen;
    p_mut.storage = StorageDescriptor::Column;
    p_mut.conversion = None;

    let t_mut = final_cat
        .tablets
        .iter_mut()
        .find(|t| t.id == tablet_id)
        .ok_or_else(|| HtapError::NotFound(format!("tablet {tablet_id} not found")))?;
    t_mut.generation = final_gen;
    t_mut.column_manifest = Some(manifest_ref);

    if let Err(err) = catalog.compare_and_set(cat3.generation, final_cat) {
        if matches!(err, HtapError::Conflict(_)) {
            // Check if another concurrent or retry operation completed the final publish
            if let Some(reloaded) = catalog.load()? {
                if let Some(p) = reloaded.partition(partition_id) {
                    if p.storage == StorageDescriptor::Column && p.conversion.is_none() {
                        if let Some(t) = reloaded.tablet(tablet_id) {
                            if let Some(mref) = &t.column_manifest {
                                if mref.generation == conv_generation
                                    && mref.base_version == snapshot_version
                                {
                                    return Ok(manifest);
                                }
                            }
                        }
                    }
                }
            }
        }
        return Err(err);
    }

    Ok(manifest)
}

/// Read visible rows for a partition, overlaying rowstore mutations on top of columnar data.
///
/// Keeps the rowstore authoritative:
/// - If the partition is in `StorageDescriptor::Row` or has no columnar manifest, reads directly
///   from rowstore at `snapshot`.
/// - If the target `snapshot` version is strictly less than the manifest `base_version`, reads
///   from rowstore historical MVCC data to avoid reading uncommitted future columnar state.
/// - If `snapshot` version is `>= manifest base_version`, reads base columnar rows from segment
///   files, then applies all rowstore mutations for this partition committed after `base_version`
///   up to `snapshot` version (Puts update/insert, Deletes tombstone/remove).
///
/// Returns rows sorted by primary key.
pub fn read_column_partition(
    catalog: &dyn CatalogStore,
    rowstore: &Engine,
    colstore_root: &Path,
    partition_id: PartitionId,
    snapshot: impl Into<Snapshot>,
) -> Result<Vec<Row>> {
    let cat_snap = catalog
        .load()?
        .ok_or_else(|| HtapError::NotFound("catalog is empty".into()))?;

    let part_desc = cat_snap
        .partition(partition_id)
        .ok_or_else(|| HtapError::NotFound(format!("partition {partition_id} not found")))?;

    let table_desc = cat_snap
        .table(part_desc.table_id)
        .ok_or_else(|| HtapError::NotFound(format!("table {} not found", part_desc.table_id)))?;

    let schema = &table_desc.schema;
    let pk_indices = &table_desc.primary_key;
    let target_snapshot = snapshot.into();

    if part_desc.tablets.is_empty() {
        return Err(HtapError::InvalidArgument(format!(
            "partition {partition_id} has no tablets"
        )));
    }
    if part_desc.tablets.len() != 1 {
        return Err(HtapError::Unsupported(format!(
            "partition {partition_id} has {} tablets; exactly 1 tablet is required",
            part_desc.tablets.len()
        )));
    }
    let tablet_id = part_desc.tablets[0];
    let tablet = cat_snap
        .tablet(tablet_id)
        .ok_or_else(|| HtapError::NotFound(format!("tablet {tablet_id} not found")))?;

    // If there is no column manifest or partition is Row, read directly from rowstore.
    if tablet.column_manifest.is_none() || matches!(part_desc.storage, StorageDescriptor::Row) {
        let entries = rowstore.scan_partition(partition_id.as_u64(), target_snapshot)?;
        return Ok(collapse_entries_to_rows(&entries));
    }

    let manifest = open(colstore_root, tablet_id)?;
    let base_version = manifest.base_version;

    // Rowstore is authoritative for historical reads before manifest base_version.
    if target_snapshot.version < base_version {
        let entries = rowstore.scan_partition(partition_id.as_u64(), target_snapshot)?;
        return Ok(collapse_entries_to_rows(&entries));
    }

    // 1. Read base rows from columnar segments into map keyed by encoded PK.
    let mut map: BTreeMap<Vec<u8>, Row> = BTreeMap::new();
    for entry in &manifest.segments {
        let seg_path = resolve_segment_path(colstore_root, tablet_id, &entry.path)?;
        let reader = SegmentReader::open(&seg_path)?;
        let req = ScanRequest::new((0..schema.len()).collect(), None);
        let scan_res = reader.scan(&req)?;
        for batch in scan_res.batches {
            let n = batch.num_rows();
            for r in 0..n {
                let mut values = Vec::with_capacity(schema.len());
                for col in &batch.columns {
                    values.push(col.get(r).ok_or_else(|| {
                        HtapError::Corruption(format!("missing value at row {r} in column vector"))
                    })?);
                }
                let row = Row::new(values);
                let pk_values: Vec<Value> = pk_indices
                    .iter()
                    .map(|&idx| {
                        row.get(idx).cloned().ok_or_else(|| {
                            HtapError::InvalidArgument(format!("row missing PK column index {idx}"))
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                let key = encode_key(&pk_values)?;
                map.insert(key, row);
            }
        }
    }

    // 2. Overlay rowstore mutations committed after base_version up to target snapshot.
    let rowstore_entries = rowstore.scan_partition(partition_id.as_u64(), target_snapshot)?;
    let mut seen_keys = HashSet::new();
    for entry in &rowstore_entries {
        let user_key = &entry.key.user_key;
        if seen_keys.contains(user_key) {
            continue;
        }
        seen_keys.insert(user_key.clone());

        if entry.key.version > base_version {
            match &entry.value {
                ValueKind::Put(row) => {
                    map.insert(user_key.clone(), row.clone());
                }
                ValueKind::Delete => {
                    map.remove(user_key);
                }
            }
        }
    }

    Ok(map.into_values().collect())
}

/// Local single-node row-to-column conversion and columnar materialization engine.
///
/// Manages conversions of partitions from row-oriented storage into columnar segments,
/// tracking catalog transitions and keeping the LSM rowstore authoritative for live mutations.
#[derive(Clone)]
pub struct LocalConverter {
    catalog: Arc<dyn CatalogStore>,
    rowstore: Arc<Engine>,
    colstore_root: PathBuf,
    options: SegmentOptions,
}

impl std::fmt::Debug for LocalConverter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalConverter")
            .field("colstore_root", &self.colstore_root)
            .field("options", &self.options)
            .finish()
    }
}

impl LocalConverter {
    /// Create a new local converter instance.
    pub fn new(
        catalog: Arc<dyn CatalogStore>,
        rowstore: Arc<Engine>,
        colstore_root: impl Into<PathBuf>,
        options: SegmentOptions,
    ) -> Self {
        Self {
            catalog,
            rowstore,
            colstore_root: colstore_root.into(),
            options,
        }
    }

    /// Access the underlying catalog store.
    pub fn catalog(&self) -> &Arc<dyn CatalogStore> {
        &self.catalog
    }

    /// Access the underlying rowstore engine.
    pub fn rowstore(&self) -> &Arc<Engine> {
        &self.rowstore
    }

    /// Access the columnar storage root directory.
    pub fn colstore_root(&self) -> &Path {
        &self.colstore_root
    }

    /// Access the segment options.
    pub fn options(&self) -> &SegmentOptions {
        &self.options
    }

    /// Perform row-to-column conversion for a partition.
    pub fn convert_partition(&self, partition_id: PartitionId) -> Result<TabletColumnManifest> {
        convert_partition(
            self.catalog.as_ref(),
            self.rowstore.as_ref(),
            &self.colstore_root,
            &self.options,
            partition_id,
        )
    }

    /// Materialize a partition to columnar format (alias for [`convert_partition`](Self::convert_partition)).
    pub fn materialize_partition(&self, partition_id: PartitionId) -> Result<TabletColumnManifest> {
        self.convert_partition(partition_id)
    }

    /// Convert a partition to columnar format (alias for [`convert_partition`](Self::convert_partition)).
    pub fn convert(&self, partition_id: PartitionId) -> Result<TabletColumnManifest> {
        self.convert_partition(partition_id)
    }

    /// Read visible rows for a partition at `snapshot`, overlaying rowstore mutations on columnar segments.
    pub fn read_column_partition(
        &self,
        partition_id: PartitionId,
        snapshot: impl Into<Snapshot>,
    ) -> Result<Vec<Row>> {
        read_column_partition(
            self.catalog.as_ref(),
            self.rowstore.as_ref(),
            &self.colstore_root,
            partition_id,
            snapshot,
        )
    }

    /// Read visible rows for a partition at the current visible snapshot of the rowstore.
    pub fn read_column_partition_current(&self, partition_id: PartitionId) -> Result<Vec<Row>> {
        self.read_column_partition(partition_id, self.rowstore.snapshot())
    }
}

/// Module providing re-exports matching module path conventions.
pub mod manifest {
    pub use super::*;
}
