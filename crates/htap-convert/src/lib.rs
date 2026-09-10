//! Row<->column conversion engine and durable tablet columnar manifests.
//!
//! Provides the crash-safe [`TabletColumnManifest`], writer helpers for columnar segments,
//! and envelope serialization with CRC32-C verification.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::HashSet;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use htap_catalog::{ColumnManifestRef, TabletId};
pub use htap_catalog::{MAX_MANIFEST_ROWS, MAX_MANIFEST_SEGMENTS};
use htap_colstore::{validate_segment_schema, SegmentOptions, SegmentReader, SegmentWriter};
use htap_common::{HtapError, Result, Row, Schema, Version};
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
pub fn resolve_segment_path(root_dir: &Path, tablet_id: TabletId, rel_path: &str) -> PathBuf {
    let t_dir = tablet_dir(root_dir, tablet_id);
    let tab_candidate = t_dir.join(rel_path);
    if tab_candidate.exists() {
        return tab_candidate;
    }
    let root_candidate = root_dir.join(rel_path);
    if root_candidate.exists() {
        return root_candidate;
    }
    tab_candidate
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

    let bytes = std::fs::read(&path)?;
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
        let seg_path = resolve_segment_path(root_dir, tablet_id, &entry.path);
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
    let bytes = std::fs::read(path)?;
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
    if segment_name.contains('/') || segment_name.contains('\\') || segment_name == ".." {
        return Err(HtapError::InvalidArgument(format!(
            "invalid segment_name '{segment_name}': must be a simple file name"
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

/// Module providing re-exports matching module path conventions.
pub mod manifest {
    pub use super::*;
}
