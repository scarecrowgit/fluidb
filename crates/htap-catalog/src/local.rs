//! Local filesystem implementation of [`CatalogStore`] with crash-safe atomic updates.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use htap_common::envelope::{decode_envelope, encode_envelope, EnvelopeError, SizeCheckMode};
use htap_common::fs::{
    atomic_publish as publish_file, remove_file_if_exists, sync_dir as sync_directory,
};
use htap_common::{read_file_exact_bounded, HtapError, Result};

use crate::model::CatalogSnapshot;
use crate::store::CatalogStore;

/// Catalog persistent file name.
pub const CATALOG_FILE_NAME: &str = "CATALOG";
/// Temporary file name used for two-phase atomic publish.
pub const CATALOG_TMP_FILE_NAME: &str = "CATALOG.tmp";
/// Header magic bytes ("HTAPCAT1").
pub const HEADER_MAGIC: &[u8; 8] = b"HTAPCAT1";
/// Catalog binary envelope format version written by this build.
///
/// Version 4 adds inline table statistics through the optional `stats` field on
/// `TableDescriptor`. Version 3 added accounts and grants, and version 2 added the persisted
/// identifier high-water mark (`id_high_water`). Versions 1 through 3 remain decodable.
pub const FORMAT_VERSION: u16 = 4;
/// Oldest catalog envelope format version this build still decodes.
pub const LEGACY_FORMAT_VERSION: u16 = 1;
/// Fixed header length (8 magic + 2 version + 4 payload_len + 4 crc32c = 18 bytes).
pub const HEADER_LEN: usize = 18;
/// Maximum allowed catalog payload size (64 MiB) to guard against unbounded allocations.
pub const MAX_CATALOG_PAYLOAD_BYTES: u32 = 64 * 1024 * 1024;

/// Local directory-backed catalog repository providing crash-consistent snapshot storage.
#[derive(Debug)]
pub struct LocalCatalogStore {
    dir: PathBuf,
    lock: Mutex<()>,
}

impl LocalCatalogStore {
    /// Open or initialize a local catalog repository at the specified directory path.
    ///
    /// If the directory does not exist, it will be created.
    /// If a published `CATALOG` file exists, its integrity is verified on startup.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;

        let store = Self {
            dir,
            lock: Mutex::new(()),
        };

        // Reopen integrity check: verify catalog file if already present.
        let _ = store.load()?;

        Ok(store)
    }

    /// Return the root directory of this local catalog store.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn read_catalog_file(&self) -> Result<Option<CatalogSnapshot>> {
        let path = self.dir.join(CATALOG_FILE_NAME);
        let max_bytes = HEADER_LEN + MAX_CATALOG_PAYLOAD_BYTES as usize;
        let bytes = match read_file_exact_bounded(&path, max_bytes) {
            Ok(b) => b,
            Err(HtapError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let snapshot = decode_snapshot(&bytes)?;
        Ok(Some(snapshot))
    }
}

impl CatalogStore for LocalCatalogStore {
    fn load(&self) -> Result<Option<CatalogSnapshot>> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.read_catalog_file()
    }

    fn compare_and_set(&self, expected_generation: u64, next: CatalogSnapshot) -> Result<()> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());

        let current = self.read_catalog_file()?;
        let current_gen = current.as_ref().map(|s| s.generation).unwrap_or(0);

        if expected_generation != current_gen {
            return Err(HtapError::Conflict(format!(
                "generation mismatch in compare_and_set: expected {expected_generation}, current {current_gen}"
            )));
        }

        if next.generation <= expected_generation {
            return Err(HtapError::InvalidArgument(format!(
                "next generation {} must be strictly greater than expected generation {}",
                next.generation, expected_generation
            )));
        }

        // Validate snapshot semantics before touching disk
        next.validate()?;

        // Once account initialization completes, it must not be reverted.
        if current
            .as_ref()
            .is_some_and(|snapshot| snapshot.accounts_initialized)
            && !next.accounts_initialized
        {
            return Err(HtapError::InvalidArgument(
                "catalog accounts_initialized flag regressed from true to false".into(),
            ));
        }

        // The identifier high-water mark must never regress: a successor that lowered it
        // would let a later allocation reissue an id whose data may still exist on disk.
        if let Some(cur) = &current {
            let (before, after) = (cur.id_high_water(), next.id_high_water());
            if after.account < before.account
                || after.table < before.table
                || after.partition < before.partition
                || after.tablet < before.tablet
                || after.replica < before.replica
            {
                return Err(HtapError::InvalidArgument(format!(
                    "catalog id high-water mark regressed: current {before:?}, next {after:?}"
                )));
            }
        }

        // Persist atomically to disk
        atomic_publish(&self.dir, &next)?;

        Ok(())
    }
}

/// Encode a catalog snapshot into versioned binary envelope bytes.
pub fn encode_snapshot(snapshot: &CatalogSnapshot) -> Result<Vec<u8>> {
    let payload = serde_json::to_vec(snapshot)
        .map_err(|e| HtapError::Internal(format!("failed to serialize catalog snapshot: {e}")))?;

    if payload.len() > MAX_CATALOG_PAYLOAD_BYTES as usize {
        return Err(HtapError::InvalidArgument(format!(
            "catalog payload size {} exceeds maximum allowed {}",
            payload.len(),
            MAX_CATALOG_PAYLOAD_BYTES
        )));
    }

    Ok(encode_envelope(HEADER_MAGIC, FORMAT_VERSION, &payload))
}

/// Decode and validate a catalog snapshot from binary envelope bytes.
pub fn decode_snapshot(bytes: &[u8]) -> Result<CatalogSnapshot> {
    let (version, payload) = decode_envelope(
        bytes,
        HEADER_MAGIC,
        LEGACY_FORMAT_VERSION..=FORMAT_VERSION,
        MAX_CATALOG_PAYLOAD_BYTES,
        SizeCheckMode::TruncatedThenTrailing,
    )
    .map_err(|err| {
        let message = match err {
            EnvelopeError::TooSmall { found, min } => {
                format!("catalog file too small: {found} bytes, minimum header size is {min}")
            }
            EnvelopeError::BadMagic => "invalid catalog header magic".into(),
            EnvelopeError::UnsupportedVersion(version) => {
                format!("unsupported catalog format version: {version}")
            }
            EnvelopeError::PayloadTooLarge { len, max } => {
                format!("catalog payload length {len} exceeds maximum limit {max}")
            }
            EnvelopeError::Truncated { expected, found } => {
                format!("truncated catalog file: expected {expected} bytes, found {found}")
            }
            EnvelopeError::TrailingBytes { extra } => {
                format!("catalog file has {extra} trailing leftover bytes")
            }
            EnvelopeError::SizeMismatch { expected, found } => {
                format!("truncated catalog file: expected {expected} bytes, found {found}")
            }
            EnvelopeError::ChecksumMismatch { expected, actual } => {
                format!("catalog checksum mismatch: expected {expected:#010x}, got {actual:#010x}")
            }
        };
        HtapError::Corruption(message)
    })?;

    let probe: serde_json::Value = serde_json::from_slice(payload).map_err(|e| {
        HtapError::Corruption(format!("failed to deserialize catalog snapshot: {e}"))
    })?;
    // Version 2 and later payloads must carry the identifier high-water mark explicitly;
    // only legacy version 1 payloads may omit it (they fall back to the live maximum).
    if version >= 2 && probe.get("id_high_water").is_none() {
        return Err(HtapError::Corruption(format!(
            "catalog format version {version} payload is missing id_high_water"
        )));
    }
    // Version 3 introduced account security state. Unlike v1/v2 compatibility defaults, a v3
    // payload must explicitly carry every security field so a damaged catalog cannot appear as
    // an uninitialized account catalog and cause bootstrap to recreate root.
    if version >= 3 {
        for key in ["accounts", "grants", "accounts_initialized"] {
            if probe.get(key).is_none() {
                return Err(HtapError::Corruption(format!(
                    "catalog format version 3 payload is missing {key}"
                )));
            }
        }
        if probe
            .get("id_high_water")
            .and_then(serde_json::Value::as_object)
            .is_none_or(|high_water| !high_water.contains_key("account"))
        {
            return Err(HtapError::Corruption(
                "catalog format version 3 payload is missing id_high_water.account".into(),
            ));
        }
    }
    let snapshot: CatalogSnapshot = serde_json::from_value(probe).map_err(|e| {
        HtapError::Corruption(format!("failed to deserialize catalog snapshot: {e}"))
    })?;

    // Validate the snapshot invariants
    snapshot.validate().map_err(|e| {
        HtapError::Corruption(format!("catalog snapshot failed integrity validation: {e}"))
    })?;

    Ok(snapshot)
}

/// Atomically publish a catalog snapshot: write `CATALOG.tmp` -> fsync -> rename -> fsync directory.
fn atomic_publish(dir: &Path, snapshot: &CatalogSnapshot) -> Result<()> {
    let tmp_path = dir.join(CATALOG_TMP_FILE_NAME);

    // Remove a stale interrupted publish before creating a restricted replacement.
    remove_file_if_exists(&tmp_path)?;

    let encoded = encode_snapshot(snapshot)?;

    publish_file(
        dir,
        CATALOG_TMP_FILE_NAME,
        CATALOG_FILE_NAME,
        &encoded,
        Some(0o600),
        true,
    )
}

/// Fsync a directory to ensure metadata operations like rename are durable.
pub fn sync_dir(path: &Path) -> Result<()> {
    sync_directory(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;
    use htap_common::{ColumnDef, DataType, Schema};

    fn test_snapshot() -> CatalogSnapshot {
        let schema = Schema::new(vec![ColumnDef {
            name: "k".to_string(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: true,
        }])
        .unwrap();

        let table = TableDescriptor::new(
            TableId::new(1),
            "test_table",
            schema,
            vec![0],
            vec![PartitionId::new(1)],
            1,
        );
        let part = PartitionDescriptor::new(
            PartitionId::new(1),
            TableId::new(1),
            "p0",
            StorageDescriptor::Row,
            vec![TabletId::new(1)],
            1,
        );
        let tab = TabletDescriptor::new(
            TabletId::new(1),
            PartitionId::new(1),
            0,
            vec![ReplicaId::new(1)],
            1,
        );
        let rep = ReplicaDescriptor::new(
            ReplicaId::new(1),
            TabletId::new(1),
            NodeId::new(1),
            true,
            true,
            1,
        );

        CatalogSnapshot::new(1, vec![table], vec![part], vec![tab], vec![rep])
    }

    // Frozen copy, do not refactor: reference encoder for the v1 payload shape.
    fn encode_v1(snapshot: &CatalogSnapshot) -> Vec<u8> {
        let mut payload = serde_json::to_value(snapshot).unwrap();
        let object = payload.as_object_mut().unwrap();
        object.remove("id_high_water");
        object.remove("accounts");
        object.remove("grants");
        object.remove("accounts_initialized");
        encode_reference_payload(1, &payload)
    }

    // Frozen copy, do not refactor: reference encoder for the v2 payload shape.
    fn encode_v2(snapshot: &CatalogSnapshot) -> Vec<u8> {
        let mut payload = serde_json::to_value(snapshot).unwrap();
        let object = payload.as_object_mut().unwrap();
        object.remove("accounts");
        object.remove("grants");
        object.remove("accounts_initialized");
        if let Some(high_water) = object
            .get_mut("id_high_water")
            .and_then(serde_json::Value::as_object_mut)
        {
            high_water.remove("account");
        }
        encode_reference_payload(2, &payload)
    }

    // Frozen copy, do not refactor: v1/v2 envelope framing.
    fn encode_reference_payload(version: u16, value: &serde_json::Value) -> Vec<u8> {
        let payload = serde_json::to_vec(value).unwrap();
        let mut bytes = Vec::with_capacity(HEADER_LEN + payload.len());
        bytes.extend_from_slice(HEADER_MAGIC);
        bytes.extend_from_slice(&version.to_le_bytes());
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&crc32c::crc32c(&payload).to_le_bytes());
        bytes.extend_from_slice(&payload);
        bytes
    }

    #[test]
    fn test_envelope_roundtrip() {
        let snap = test_snapshot();
        let bytes = encode_snapshot(&snap).unwrap();
        let decoded = decode_snapshot(&bytes).unwrap();
        assert_eq!(decoded, snap);
    }

    #[test]
    fn catalog_envelope_golden_bytes() {
        let snapshot = test_snapshot();
        let v1 = encode_v1(&snapshot);
        let v2 = encode_v2(&snapshot);
        let v4 = encode_snapshot(&snapshot).unwrap();

        assert_eq!(
            v1,
            vec![
                72, 84, 65, 80, 67, 65, 84, 49, 1, 0, 210, 1, 0, 0, 141, 99, 75, 186, 123, 34, 103,
                101, 110, 101, 114, 97, 116, 105, 111, 110, 34, 58, 49, 44, 34, 112, 97, 114, 116,
                105, 116, 105, 111, 110, 115, 34, 58, 91, 123, 34, 103, 101, 110, 101, 114, 97,
                116, 105, 111, 110, 34, 58, 49, 44, 34, 105, 100, 34, 58, 49, 44, 34, 110, 97, 109,
                101, 34, 58, 34, 112, 48, 34, 44, 34, 115, 116, 111, 114, 97, 103, 101, 34, 58, 34,
                82, 111, 119, 34, 44, 34, 116, 97, 98, 108, 101, 95, 105, 100, 34, 58, 49, 44, 34,
                116, 97, 98, 108, 101, 116, 115, 34, 58, 91, 49, 93, 125, 93, 44, 34, 114, 101,
                112, 108, 105, 99, 97, 115, 34, 58, 91, 123, 34, 103, 101, 110, 101, 114, 97, 116,
                105, 111, 110, 34, 58, 49, 44, 34, 104, 101, 97, 108, 116, 104, 121, 34, 58, 116,
                114, 117, 101, 44, 34, 105, 100, 34, 58, 49, 44, 34, 105, 115, 95, 108, 101, 97,
                100, 101, 114, 34, 58, 116, 114, 117, 101, 44, 34, 110, 111, 100, 101, 95, 105,
                100, 34, 58, 49, 44, 34, 116, 97, 98, 108, 101, 116, 95, 105, 100, 34, 58, 49, 125,
                93, 44, 34, 116, 97, 98, 108, 101, 115, 34, 58, 91, 123, 34, 103, 101, 110, 101,
                114, 97, 116, 105, 111, 110, 34, 58, 49, 44, 34, 105, 100, 34, 58, 49, 44, 34, 110,
                97, 109, 101, 34, 58, 34, 116, 101, 115, 116, 95, 116, 97, 98, 108, 101, 34, 44,
                34, 112, 97, 114, 116, 105, 116, 105, 111, 110, 115, 34, 58, 91, 49, 93, 44, 34,
                112, 114, 105, 109, 97, 114, 121, 95, 107, 101, 121, 34, 58, 91, 48, 93, 44, 34,
                115, 99, 104, 101, 109, 97, 34, 58, 123, 34, 99, 111, 108, 117, 109, 110, 115, 34,
                58, 91, 123, 34, 100, 97, 116, 97, 95, 116, 121, 112, 101, 34, 58, 34, 73, 110,
                116, 54, 52, 34, 44, 34, 110, 97, 109, 101, 34, 58, 34, 107, 34, 44, 34, 110, 117,
                108, 108, 97, 98, 108, 101, 34, 58, 102, 97, 108, 115, 101, 44, 34, 112, 114, 105,
                109, 97, 114, 121, 95, 107, 101, 121, 34, 58, 116, 114, 117, 101, 125, 93, 125,
                125, 93, 44, 34, 116, 97, 98, 108, 101, 116, 115, 34, 58, 91, 123, 34, 98, 117, 99,
                107, 101, 116, 34, 58, 48, 44, 34, 103, 101, 110, 101, 114, 97, 116, 105, 111, 110,
                34, 58, 49, 44, 34, 105, 100, 34, 58, 49, 44, 34, 112, 97, 114, 116, 105, 116, 105,
                111, 110, 95, 105, 100, 34, 58, 49, 44, 34, 114, 101, 112, 108, 105, 99, 97, 115,
                34, 58, 91, 49, 93, 125, 93, 125,
            ]
        );
        assert_eq!(
            v2,
            vec![
                72, 84, 65, 80, 67, 65, 84, 49, 2, 0, 19, 2, 0, 0, 173, 42, 182, 184, 123, 34, 103,
                101, 110, 101, 114, 97, 116, 105, 111, 110, 34, 58, 49, 44, 34, 105, 100, 95, 104,
                105, 103, 104, 95, 119, 97, 116, 101, 114, 34, 58, 123, 34, 112, 97, 114, 116, 105,
                116, 105, 111, 110, 34, 58, 48, 44, 34, 114, 101, 112, 108, 105, 99, 97, 34, 58,
                48, 44, 34, 116, 97, 98, 108, 101, 34, 58, 48, 44, 34, 116, 97, 98, 108, 101, 116,
                34, 58, 48, 125, 44, 34, 112, 97, 114, 116, 105, 116, 105, 111, 110, 115, 34, 58,
                91, 123, 34, 103, 101, 110, 101, 114, 97, 116, 105, 111, 110, 34, 58, 49, 44, 34,
                105, 100, 34, 58, 49, 44, 34, 110, 97, 109, 101, 34, 58, 34, 112, 48, 34, 44, 34,
                115, 116, 111, 114, 97, 103, 101, 34, 58, 34, 82, 111, 119, 34, 44, 34, 116, 97,
                98, 108, 101, 95, 105, 100, 34, 58, 49, 44, 34, 116, 97, 98, 108, 101, 116, 115,
                34, 58, 91, 49, 93, 125, 93, 44, 34, 114, 101, 112, 108, 105, 99, 97, 115, 34, 58,
                91, 123, 34, 103, 101, 110, 101, 114, 97, 116, 105, 111, 110, 34, 58, 49, 44, 34,
                104, 101, 97, 108, 116, 104, 121, 34, 58, 116, 114, 117, 101, 44, 34, 105, 100, 34,
                58, 49, 44, 34, 105, 115, 95, 108, 101, 97, 100, 101, 114, 34, 58, 116, 114, 117,
                101, 44, 34, 110, 111, 100, 101, 95, 105, 100, 34, 58, 49, 44, 34, 116, 97, 98,
                108, 101, 116, 95, 105, 100, 34, 58, 49, 125, 93, 44, 34, 116, 97, 98, 108, 101,
                115, 34, 58, 91, 123, 34, 103, 101, 110, 101, 114, 97, 116, 105, 111, 110, 34, 58,
                49, 44, 34, 105, 100, 34, 58, 49, 44, 34, 110, 97, 109, 101, 34, 58, 34, 116, 101,
                115, 116, 95, 116, 97, 98, 108, 101, 34, 44, 34, 112, 97, 114, 116, 105, 116, 105,
                111, 110, 115, 34, 58, 91, 49, 93, 44, 34, 112, 114, 105, 109, 97, 114, 121, 95,
                107, 101, 121, 34, 58, 91, 48, 93, 44, 34, 115, 99, 104, 101, 109, 97, 34, 58, 123,
                34, 99, 111, 108, 117, 109, 110, 115, 34, 58, 91, 123, 34, 100, 97, 116, 97, 95,
                116, 121, 112, 101, 34, 58, 34, 73, 110, 116, 54, 52, 34, 44, 34, 110, 97, 109,
                101, 34, 58, 34, 107, 34, 44, 34, 110, 117, 108, 108, 97, 98, 108, 101, 34, 58,
                102, 97, 108, 115, 101, 44, 34, 112, 114, 105, 109, 97, 114, 121, 95, 107, 101,
                121, 34, 58, 116, 114, 117, 101, 125, 93, 125, 125, 93, 44, 34, 116, 97, 98, 108,
                101, 116, 115, 34, 58, 91, 123, 34, 98, 117, 99, 107, 101, 116, 34, 58, 48, 44, 34,
                103, 101, 110, 101, 114, 97, 116, 105, 111, 110, 34, 58, 49, 44, 34, 105, 100, 34,
                58, 49, 44, 34, 112, 97, 114, 116, 105, 116, 105, 111, 110, 95, 105, 100, 34, 58,
                49, 44, 34, 114, 101, 112, 108, 105, 99, 97, 115, 34, 58, 91, 49, 93, 125, 93, 125,
            ]
        );
        assert_eq!(
            v4,
            vec![
                72, 84, 65, 80, 67, 65, 84, 49, 4, 0, 86, 2, 0, 0, 11, 97, 133, 107, 123, 34, 103,
                101, 110, 101, 114, 97, 116, 105, 111, 110, 34, 58, 49, 44, 34, 97, 99, 99, 111,
                117, 110, 116, 115, 34, 58, 91, 93, 44, 34, 103, 114, 97, 110, 116, 115, 34, 58,
                91, 93, 44, 34, 97, 99, 99, 111, 117, 110, 116, 115, 95, 105, 110, 105, 116, 105,
                97, 108, 105, 122, 101, 100, 34, 58, 102, 97, 108, 115, 101, 44, 34, 116, 97, 98,
                108, 101, 115, 34, 58, 91, 123, 34, 105, 100, 34, 58, 49, 44, 34, 110, 97, 109,
                101, 34, 58, 34, 116, 101, 115, 116, 95, 116, 97, 98, 108, 101, 34, 44, 34, 115,
                99, 104, 101, 109, 97, 34, 58, 123, 34, 99, 111, 108, 117, 109, 110, 115, 34, 58,
                91, 123, 34, 110, 97, 109, 101, 34, 58, 34, 107, 34, 44, 34, 100, 97, 116, 97, 95,
                116, 121, 112, 101, 34, 58, 34, 73, 110, 116, 54, 52, 34, 44, 34, 110, 117, 108,
                108, 97, 98, 108, 101, 34, 58, 102, 97, 108, 115, 101, 44, 34, 112, 114, 105, 109,
                97, 114, 121, 95, 107, 101, 121, 34, 58, 116, 114, 117, 101, 125, 93, 125, 44, 34,
                112, 114, 105, 109, 97, 114, 121, 95, 107, 101, 121, 34, 58, 91, 48, 93, 44, 34,
                112, 97, 114, 116, 105, 116, 105, 111, 110, 115, 34, 58, 91, 49, 93, 44, 34, 103,
                101, 110, 101, 114, 97, 116, 105, 111, 110, 34, 58, 49, 125, 93, 44, 34, 112, 97,
                114, 116, 105, 116, 105, 111, 110, 115, 34, 58, 91, 123, 34, 105, 100, 34, 58, 49,
                44, 34, 116, 97, 98, 108, 101, 95, 105, 100, 34, 58, 49, 44, 34, 110, 97, 109, 101,
                34, 58, 34, 112, 48, 34, 44, 34, 115, 116, 111, 114, 97, 103, 101, 34, 58, 34, 82,
                111, 119, 34, 44, 34, 116, 97, 98, 108, 101, 116, 115, 34, 58, 91, 49, 93, 44, 34,
                103, 101, 110, 101, 114, 97, 116, 105, 111, 110, 34, 58, 49, 125, 93, 44, 34, 116,
                97, 98, 108, 101, 116, 115, 34, 58, 91, 123, 34, 105, 100, 34, 58, 49, 44, 34, 112,
                97, 114, 116, 105, 116, 105, 111, 110, 95, 105, 100, 34, 58, 49, 44, 34, 98, 117,
                99, 107, 101, 116, 34, 58, 48, 44, 34, 114, 101, 112, 108, 105, 99, 97, 115, 34,
                58, 91, 49, 93, 44, 34, 103, 101, 110, 101, 114, 97, 116, 105, 111, 110, 34, 58,
                49, 125, 93, 44, 34, 114, 101, 112, 108, 105, 99, 97, 115, 34, 58, 91, 123, 34,
                105, 100, 34, 58, 49, 44, 34, 116, 97, 98, 108, 101, 116, 95, 105, 100, 34, 58, 49,
                44, 34, 110, 111, 100, 101, 95, 105, 100, 34, 58, 49, 44, 34, 105, 115, 95, 108,
                101, 97, 100, 101, 114, 34, 58, 116, 114, 117, 101, 44, 34, 104, 101, 97, 108, 116,
                104, 121, 34, 58, 116, 114, 117, 101, 44, 34, 103, 101, 110, 101, 114, 97, 116,
                105, 111, 110, 34, 58, 49, 125, 93, 44, 34, 105, 100, 95, 104, 105, 103, 104, 95,
                119, 97, 116, 101, 114, 34, 58, 123, 34, 97, 99, 99, 111, 117, 110, 116, 34, 58,
                48, 44, 34, 116, 97, 98, 108, 101, 34, 58, 48, 44, 34, 112, 97, 114, 116, 105, 116,
                105, 111, 110, 34, 58, 48, 44, 34, 116, 97, 98, 108, 101, 116, 34, 58, 48, 44, 34,
                114, 101, 112, 108, 105, 99, 97, 34, 58, 48, 125, 125,
            ]
        );

        assert_eq!(
            decode_snapshot(&v1).unwrap().generation,
            snapshot.generation
        );
        assert_eq!(
            decode_snapshot(&v2).unwrap().generation,
            snapshot.generation
        );
        assert_eq!(decode_snapshot(&v4).unwrap(), snapshot);
    }

    #[test]
    fn catalog_envelope_bad_magic_and_oversized() {
        let mut bad_magic = encode_snapshot(&test_snapshot()).unwrap();
        bad_magic[0] ^= 0xff;
        bad_magic[10..14].copy_from_slice(&(MAX_CATALOG_PAYLOAD_BYTES + 1).to_le_bytes());
        let err = decode_snapshot(&bad_magic).expect_err("bad magic must fail");
        assert_eq!(
            err.to_string(),
            "Corruption error: invalid catalog header magic"
        );

        let mut oversized = vec![0; HEADER_LEN];
        oversized[..8].copy_from_slice(HEADER_MAGIC);
        oversized[8..10].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        oversized[10..14].copy_from_slice(&(MAX_CATALOG_PAYLOAD_BYTES + 1).to_le_bytes());
        let err = decode_snapshot(&oversized).expect_err("oversized payload must fail");
        assert_eq!(
            err.to_string(),
            format!(
                "Corruption error: catalog payload length {} exceeds maximum limit {}",
                MAX_CATALOG_PAYLOAD_BYTES + 1,
                MAX_CATALOG_PAYLOAD_BYTES
            )
        );
    }

    #[test]
    fn catalog_envelope_bad_version_and_crc() {
        let mut bad_version = encode_snapshot(&test_snapshot()).unwrap();
        bad_version[8..10].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
        bad_version[14] ^= 0xff;
        let err = decode_snapshot(&bad_version).expect_err("unsupported version must fail");
        assert_eq!(
            err.to_string(),
            format!(
                "Corruption error: unsupported catalog format version: {}",
                FORMAT_VERSION + 1
            )
        );

        let mut bad_crc = encode_snapshot(&test_snapshot()).unwrap();
        bad_crc[14] ^= 0xff;
        let err = decode_snapshot(&bad_crc).expect_err("bad CRC must fail");
        assert_eq!(
            err.to_string(),
            "Corruption error: catalog checksum mismatch: expected 0x6b8561f4, got 0x6b85610b"
        );
    }

    #[test]
    fn catalog_envelope_size_check_truncated() {
        let mut bytes = encode_snapshot(&test_snapshot()).unwrap();
        let expected = bytes.len();
        bytes.pop();
        let found = bytes.len();
        let err = decode_snapshot(&bytes).expect_err("truncated envelope must fail");
        assert_eq!(
            err.to_string(),
            format!(
                "Corruption error: truncated catalog file: expected {expected} bytes, found {found}"
            )
        );
    }

    #[test]
    fn catalog_envelope_size_check_trailing() {
        let mut bytes = encode_snapshot(&test_snapshot()).unwrap();
        bytes.push(0);
        let err = decode_snapshot(&bytes).expect_err("trailing byte must fail");
        assert_eq!(
            err.to_string(),
            "Corruption error: catalog file has 1 trailing leftover bytes"
        );
    }
}
