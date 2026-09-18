//! Local filesystem implementation of [`CatalogStore`] with crash-safe atomic updates.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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
/// Version 3 adds accounts and grants to the JSON payload. Version 2 added the persisted
/// identifier high-water mark (`id_high_water`). Versions 1 and 2 remain decodable.
pub const FORMAT_VERSION: u16 = 3;
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

/// Decode and validate a catalog snapshot from binary envelope bytes.
pub fn decode_snapshot(bytes: &[u8]) -> Result<CatalogSnapshot> {
    if bytes.len() < HEADER_LEN {
        return Err(HtapError::Corruption(format!(
            "catalog file too small: {} bytes, minimum header size is {}",
            bytes.len(),
            HEADER_LEN
        )));
    }

    if &bytes[0..8] != HEADER_MAGIC {
        return Err(HtapError::Corruption("invalid catalog header magic".into()));
    }

    let version = u16::from_le_bytes(bytes[8..10].try_into().unwrap());
    if !(LEGACY_FORMAT_VERSION..=FORMAT_VERSION).contains(&version) {
        return Err(HtapError::Corruption(format!(
            "unsupported catalog format version: {version}"
        )));
    }

    let payload_len = u32::from_le_bytes(bytes[10..14].try_into().unwrap());
    let expected_crc = u32::from_le_bytes(bytes[14..18].try_into().unwrap());

    if payload_len > MAX_CATALOG_PAYLOAD_BYTES {
        return Err(HtapError::Corruption(format!(
            "catalog payload length {} exceeds maximum limit {}",
            payload_len, MAX_CATALOG_PAYLOAD_BYTES
        )));
    }

    let expected_total = HEADER_LEN + payload_len as usize;
    if bytes.len() < expected_total {
        return Err(HtapError::Corruption(format!(
            "truncated catalog file: expected {} bytes, found {}",
            expected_total,
            bytes.len()
        )));
    }

    if bytes.len() > expected_total {
        return Err(HtapError::Corruption(format!(
            "catalog file has {} trailing leftover bytes",
            bytes.len() - expected_total
        )));
    }

    let payload = &bytes[HEADER_LEN..expected_total];
    let computed_crc = crc32c::crc32c(payload);
    if computed_crc != expected_crc {
        return Err(HtapError::Corruption(format!(
            "catalog checksum mismatch: expected {expected_crc:#010x}, got {computed_crc:#010x}"
        )));
    }

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
    let final_path = dir.join(CATALOG_FILE_NAME);

    // Remove a stale interrupted publish before creating a restricted replacement.
    match fs::remove_file(&tmp_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(HtapError::Io(e)),
    }

    let encoded = encode_snapshot(snapshot)?;

    // 1. Write tmp file and fsync
    let write_res = (|| -> Result<()> {
        let mut file = {
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;

                OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&tmp_path)?
            }
            #[cfg(not(unix))]
            {
                File::create(&tmp_path)?
            }
        };
        file.write_all(&encoded)?;
        file.sync_all()?;
        Ok(())
    })();

    if let Err(e) = write_res {
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }

    // 2. Atomic rename
    if let Err(e) = fs::rename(&tmp_path, &final_path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(HtapError::Io(e));
    }

    // 3. Fsync parent directory
    sync_dir(dir)?;

    Ok(())
}

/// Fsync a directory to ensure metadata operations like rename are durable.
pub fn sync_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let f = File::open(path)?;
        f.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
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

    #[test]
    fn test_envelope_roundtrip() {
        let snap = test_snapshot();
        let bytes = encode_snapshot(&snap).unwrap();
        let decoded = decode_snapshot(&bytes).unwrap();
        assert_eq!(decoded, snap);
    }
}
