//! Authoritative manifest for published SST files.
//!
//! The manifest is the sole authority on which SSTs are live in the LSM engine.
//! A directory scan cannot distinguish an fsynced-but-unpublished SST from a
//! published one after a crash; therefore, only SST files registered in the
//! manifest are opened during recovery.
//!
//! # Format
//!
//! All integers are little-endian.
//!
//! ```text
//! magic: [u8; 8] = "HTAPMAN1"
//! format_version: u16 = 1
//! payload_len: u32
//! crc32c: u32              // CRC32-C over payload bytes only
//! payload: [u8; payload_len]
//! ```
//!
//! Payload encoding:
//! ```text
//! sst_count: u32
//! sst_entry*
//!
//! sst_entry :=
//!   id: u64
//!   entry_count: u64
//!   has_min_version: u8    // 0 = None, 1 = Some
//!   min_version: u64       // present if has_min_version == 1
//!   has_max_version: u8    // 0 = None, 1 = Some
//!   max_version: u64       // present if has_max_version == 1
//! ```
//!
//! # Atomic Publication
//!
//! Updating the manifest follows a crash-consistent two-phase atomic publish:
//! 1. Write the new manifest to `MANIFEST.tmp`.
//! 2. Fsync `MANIFEST.tmp`.
//! 3. Atomic rename of `MANIFEST.tmp` to `MANIFEST`.
//! 4. Fsync the parent directory.

use std::io::Write;
use std::path::Path;

use htap_common::{HtapError, Result, Version};

use crate::sst::SstMetadata;

/// Header magic bytes ("HTAPMAN1").
const HEADER_MAGIC: &[u8; 8] = b"HTAPMAN1";

/// Current manifest format version.
const FORMAT_VERSION: u16 = 1;

/// Fixed manifest header size (8 magic + 2 format + 4 payload_len + 4 crc32c = 18 bytes).
const HEADER_LEN: usize = 18;

/// Upper bound on manifest payload size (64 MiB) to guard against unbounded allocations.
pub const MAX_MANIFEST_PAYLOAD_BYTES: u32 = 64 * 1024 * 1024;

/// Upper bound on the number of SSTs recorded in the manifest (1 million).
pub const MAX_SST_COUNT: u32 = 1_000_000;

/// Summary metadata for an SST file registered in the manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestSstEntry {
    /// Unique identifier of the SST file (stored as `<id>.sst`).
    pub id: u64,
    /// Number of entries stored in the SST file.
    pub entry_count: u64,
    /// Smallest commit version in the SST, if non-empty.
    pub min_version: Option<Version>,
    /// Largest commit version in the SST, if non-empty.
    pub max_version: Option<Version>,
}

impl From<&SstMetadata> for ManifestSstEntry {
    fn from(meta: &SstMetadata) -> Self {
        Self {
            id: meta.id,
            entry_count: meta.entry_count,
            min_version: meta.min_version,
            max_version: meta.max_version,
        }
    }
}

/// Authoritative record of published SSTs in the LSM store.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Manifest {
    /// Ordered list of registered SSTs, newest first.
    pub ssts: Vec<ManifestSstEntry>,
}

impl Manifest {
    /// Create a new empty manifest.
    pub fn new() -> Self {
        Self { ssts: Vec::new() }
    }

    /// Prepend a newly published SST to the manifest.
    pub fn prepend(&mut self, entry: ManifestSstEntry) {
        self.ssts.insert(0, entry);
    }

    /// Encode the manifest to bytes including header, format version, and CRC32-C.
    pub fn encode(&self) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&(self.ssts.len() as u32).to_le_bytes());
        for sst in &self.ssts {
            payload.extend_from_slice(&sst.id.to_le_bytes());
            payload.extend_from_slice(&sst.entry_count.to_le_bytes());
            if let Some(min_v) = sst.min_version {
                payload.push(1);
                payload.extend_from_slice(&min_v.get().to_le_bytes());
            } else {
                payload.push(0);
            }
            if let Some(max_v) = sst.max_version {
                payload.push(1);
                payload.extend_from_slice(&max_v.get().to_le_bytes());
            } else {
                payload.push(0);
            }
        }

        let payload_len = payload.len() as u32;
        let crc = crc32c::crc32c(&payload);

        let mut buf = Vec::with_capacity(HEADER_LEN + payload.len());
        buf.extend_from_slice(HEADER_MAGIC);
        buf.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        buf.extend_from_slice(&payload_len.to_le_bytes());
        buf.extend_from_slice(&crc.to_le_bytes());
        buf.extend_from_slice(&payload);
        buf
    }

    /// Decode a manifest from serialized bytes.
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

        let total_expected = HEADER_LEN + payload_len as usize;
        if bytes.len() != total_expected {
            return Err(HtapError::Corruption(format!(
                "manifest size mismatch: expected {total_expected} bytes, got {}",
                bytes.len()
            )));
        }

        let payload = &bytes[HEADER_LEN..total_expected];
        let actual_crc = crc32c::crc32c(payload);
        if actual_crc != expected_crc {
            return Err(HtapError::Corruption(format!(
                "manifest CRC mismatch: expected {expected_crc:#010x}, calculated {actual_crc:#010x}"
            )));
        }

        let mut cursor = payload;
        if cursor.len() < 4 {
            return Err(HtapError::Corruption(
                "manifest payload too short for SST count".into(),
            ));
        }
        let sst_count = u32::from_le_bytes(cursor[0..4].try_into().unwrap());
        cursor = &cursor[4..];

        if sst_count > MAX_SST_COUNT {
            return Err(HtapError::Corruption(format!(
                "manifest SST count {sst_count} exceeds maximum {MAX_SST_COUNT}"
            )));
        }

        let mut ssts = Vec::with_capacity(sst_count as usize);
        for _ in 0..sst_count {
            if cursor.len() < 16 {
                return Err(HtapError::Corruption(
                    "truncated SST entry in manifest".into(),
                ));
            }
            let id = u64::from_le_bytes(cursor[0..8].try_into().unwrap());
            let entry_count = u64::from_le_bytes(cursor[8..16].try_into().unwrap());
            cursor = &cursor[16..];

            if cursor.is_empty() {
                return Err(HtapError::Corruption(
                    "truncated min_version tag in manifest".into(),
                ));
            }
            let has_min = cursor[0];
            cursor = &cursor[1..];
            let min_version = if has_min == 1 {
                if cursor.len() < 8 {
                    return Err(HtapError::Corruption(
                        "truncated min_version in manifest".into(),
                    ));
                }
                let v = u64::from_le_bytes(cursor[0..8].try_into().unwrap());
                cursor = &cursor[8..];
                Some(Version::new(v))
            } else if has_min == 0 {
                None
            } else {
                return Err(HtapError::Corruption(format!(
                    "invalid min_version tag in manifest: {has_min}"
                )));
            };

            if cursor.is_empty() {
                return Err(HtapError::Corruption(
                    "truncated max_version tag in manifest".into(),
                ));
            }
            let has_max = cursor[0];
            cursor = &cursor[1..];
            let max_version = if has_max == 1 {
                if cursor.len() < 8 {
                    return Err(HtapError::Corruption(
                        "truncated max_version in manifest".into(),
                    ));
                }
                let v = u64::from_le_bytes(cursor[0..8].try_into().unwrap());
                cursor = &cursor[8..];
                Some(Version::new(v))
            } else if has_max == 0 {
                None
            } else {
                return Err(HtapError::Corruption(format!(
                    "invalid max_version tag in manifest: {has_max}"
                )));
            };

            ssts.push(ManifestSstEntry {
                id,
                entry_count,
                min_version,
                max_version,
            });
        }

        if !cursor.is_empty() {
            return Err(HtapError::Corruption(format!(
                "manifest has {} trailing leftover bytes",
                cursor.len()
            )));
        }

        Ok(Manifest { ssts })
    }

    /// Read a manifest from a file path. Returns `Ok(None)` if file does not exist.
    pub fn read_from_file(path: &Path) -> Result<Option<Self>> {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(HtapError::Io(e)),
        };
        let manifest = Self::decode(&bytes)?;
        Ok(Some(manifest))
    }

    /// Atomically publish the manifest: write `dir/MANIFEST.tmp` -> fsync -> rename -> fsync `dir`.
    pub fn atomic_publish(dir: &Path, manifest: &Manifest) -> Result<()> {
        let tmp_path = dir.join("MANIFEST.tmp");
        let final_path = dir.join("MANIFEST");

        let encoded = manifest.encode();
        {
            let mut file = std::fs::File::create(&tmp_path)?;
            file.write_all(&encoded)?;
            file.sync_all()?;
        }

        std::fs::rename(&tmp_path, &final_path)?;
        sync_dir(dir)?;
        Ok(())
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_manifest_empty_round_trip() {
        let manifest = Manifest::new();
        let bytes = manifest.encode();
        let decoded = Manifest::decode(&bytes).unwrap();
        assert_eq!(manifest, decoded);
    }

    #[test]
    fn test_manifest_round_trip_with_entries() {
        let mut manifest = Manifest::new();
        manifest.prepend(ManifestSstEntry {
            id: 1,
            entry_count: 100,
            min_version: Some(Version::new(2)),
            max_version: Some(Version::new(10)),
        });
        manifest.prepend(ManifestSstEntry {
            id: 2,
            entry_count: 50,
            min_version: None,
            max_version: None,
        });

        let bytes = manifest.encode();
        let decoded = Manifest::decode(&bytes).unwrap();
        assert_eq!(manifest, decoded);
    }

    #[test]
    fn test_manifest_crc_corruption() {
        let mut manifest = Manifest::new();
        manifest.prepend(ManifestSstEntry {
            id: 1,
            entry_count: 10,
            min_version: Some(Version::new(2)),
            max_version: Some(Version::new(5)),
        });
        let mut bytes = manifest.encode();
        // Flip a byte in the payload
        let last_idx = bytes.len() - 1;
        bytes[last_idx] ^= 0xff;

        assert!(matches!(
            Manifest::decode(&bytes),
            Err(HtapError::Corruption(_))
        ));
    }

    #[test]
    fn test_manifest_magic_corruption() {
        let manifest = Manifest::new();
        let mut bytes = manifest.encode();
        bytes[0] = b'X';
        assert!(matches!(
            Manifest::decode(&bytes),
            Err(HtapError::Corruption(_))
        ));
    }

    #[test]
    fn test_manifest_atomic_publish_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::new();
        manifest.prepend(ManifestSstEntry {
            id: 42,
            entry_count: 10,
            min_version: Some(Version::new(3)),
            max_version: Some(Version::new(8)),
        });

        Manifest::atomic_publish(dir.path(), &manifest).unwrap();
        let read = Manifest::read_from_file(&dir.path().join("MANIFEST"))
            .unwrap()
            .unwrap();
        assert_eq!(manifest, read);
    }
}
