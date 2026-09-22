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
//! format_version: u16 = 2
//! payload_len: u32
//! crc32c: u32              // CRC32-C over payload bytes only
//! payload: [u8; payload_len]
//! ```
//!
//! Payload encoding (v2):
//! ```text
//! sst_count: u32
//! sst_entry*
//! ledger_count: u32
//! ledger_entry*
//!
//! sst_entry :=
//!   id: u64
//!   entry_count: u64
//!   has_min_version: u8    // 0 = None, 1 = Some
//!   min_version: u64       // present if has_min_version == 1
//!   has_max_version: u8    // 0 = None, 1 = Some
//!   max_version: u64       // present if has_max_version == 1
//!
//! ledger_entry :=
//!   txn_id: u64
//!   version: u64
//! ```
//!
//! # Format Compatibility
//!
//! Format v1 manifests contain only the SST sequence. Format v2 adds the external
//! transaction ledger. Their committed-version high-water is derived from SST metadata
//! and their GC low-water defaults to [`Version::INITIAL`]. New manifests are written
//! in format v3 with both watermarks encoded after the ledger.
//!
//! # Atomic Publication
//!
//! Updating the manifest follows a crash-consistent two-phase atomic publish:
//! 1. Write the new manifest to `MANIFEST.tmp`.
//! 2. Fsync `MANIFEST.tmp`.
//! 3. Atomic rename of `MANIFEST.tmp` to `MANIFEST`.
//! 4. Fsync the parent directory.

use std::path::Path;

use htap_common::envelope::{decode_envelope, encode_envelope, SizeCheckMode};
use htap_common::fs::{atomic_publish, read_file_exact_bounded_from_file};
use htap_common::{HtapError, Result, Version};

use crate::sst::SstMetadata;

/// Header magic bytes ("HTAPMAN1").
const HEADER_MAGIC: &[u8; 8] = b"HTAPMAN1";

/// Manifest format version 1 (legacy, SST sequence only).
pub const FORMAT_VERSION_V1: u16 = 1;

/// Manifest format version 2 (SST sequence + external transaction ledger).
pub const FORMAT_VERSION_V2: u16 = 2;

/// Manifest format version 3 (SST sequence, ledger, and version watermarks).
pub const FORMAT_VERSION_V3: u16 = 3;

/// Current manifest format version.
pub const FORMAT_VERSION: u16 = FORMAT_VERSION_V3;

/// Fixed manifest header size (8 magic + 2 format + 4 payload_len + 4 crc32c = 18 bytes).
const HEADER_LEN: usize = 18;

/// Upper bound on manifest payload size (64 MiB) to guard against unbounded allocations.
pub const MAX_MANIFEST_PAYLOAD_BYTES: u32 = 64 * 1024 * 1024;

/// Upper bound on the number of SSTs recorded in the manifest (1 million).
pub const MAX_SST_COUNT: u32 = 1_000_000;

/// Hard cap on the number of external applied transactions recorded in the manifest ledger (1 million).
///
/// Ledger entries are never evicted; once this hard cap is reached, new external transactions
/// are rejected before modifying any engine state.
pub const MAX_APPLIED_EXTERNAL_TXNS: usize = 1_000_000;

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

/// An external transaction recorded in the manifest ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestLedgerEntry {
    /// Unique external transaction identifier (nonzero).
    pub txn_id: u64,
    /// Commit version assigned to the transaction.
    pub version: Version,
}

impl ManifestLedgerEntry {
    /// Create a new manifest ledger entry.
    pub fn new(txn_id: u64, version: Version) -> Self {
        Self { txn_id, version }
    }
}

/// Authoritative record of published SSTs and applied external transactions in the LSM store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// Ordered list of registered SSTs, newest first.
    pub ssts: Vec<ManifestSstEntry>,
    /// Complete ledger of applied external transactions.
    pub applied_txns: Vec<ManifestLedgerEntry>,
    /// Highest committed version covered by a published manifest.
    pub committed_version_high_water: Version,
    /// Oldest snapshot version whose history is retained.
    pub gc_low_water: Version,
}

impl Manifest {
    /// Create a new empty manifest.
    pub fn new() -> Self {
        Self {
            ssts: Vec::new(),
            applied_txns: Vec::new(),
            committed_version_high_water: Version::INITIAL,
            gc_low_water: Version::INITIAL,
        }
    }

    /// Create a manifest with existing SSTs and applied transactions ledger.
    pub fn with_ledger(
        ssts: Vec<ManifestSstEntry>,
        applied_txns: Vec<ManifestLedgerEntry>,
    ) -> Self {
        let committed_version_high_water = ssts
            .iter()
            .filter_map(|sst| sst.max_version)
            .max()
            .unwrap_or(Version::INITIAL);
        Self {
            ssts,
            applied_txns,
            committed_version_high_water,
            gc_low_water: Version::INITIAL,
        }
    }

    /// Prepend a newly published SST to the manifest.
    pub fn prepend(&mut self, entry: ManifestSstEntry) {
        self.ssts.insert(0, entry);
    }

    /// Encode the manifest to bytes in format v3 including header, format version, and CRC32-C.
    pub fn encode(&self) -> Result<Vec<u8>> {
        if self.ssts.len() > MAX_SST_COUNT as usize {
            return Err(HtapError::InvalidArgument(format!(
                "manifest SST count {} exceeds maximum {MAX_SST_COUNT}",
                self.ssts.len()
            )));
        }
        if self.applied_txns.len() > MAX_APPLIED_EXTERNAL_TXNS {
            return Err(HtapError::InvalidArgument(format!(
                "manifest applied_txns {} exceeds maximum {MAX_APPLIED_EXTERNAL_TXNS}",
                self.applied_txns.len()
            )));
        }

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

        // Append external ledger count and entries.
        payload.extend_from_slice(&(self.applied_txns.len() as u32).to_le_bytes());
        for entry in &self.applied_txns {
            payload.extend_from_slice(&entry.txn_id.to_le_bytes());
            payload.extend_from_slice(&entry.version.get().to_le_bytes());
        }
        payload.extend_from_slice(&self.committed_version_high_water.get().to_le_bytes());
        payload.extend_from_slice(&self.gc_low_water.get().to_le_bytes());

        let payload_len = u32::try_from(payload.len()).map_err(|_| {
            HtapError::InvalidArgument("manifest payload length exceeds u32".into())
        })?;
        if payload_len > MAX_MANIFEST_PAYLOAD_BYTES {
            return Err(HtapError::InvalidArgument(format!(
                "manifest payload length {payload_len} exceeds maximum {MAX_MANIFEST_PAYLOAD_BYTES}"
            )));
        }
        Ok(encode_envelope(HEADER_MAGIC, FORMAT_VERSION_V3, &payload))
    }

    /// Encode the manifest to bytes in legacy format v1 (SST sequence only, no external ledger).
    pub fn encode_v1(&self) -> Vec<u8> {
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

        encode_envelope(HEADER_MAGIC, FORMAT_VERSION_V1, &payload)
    }

    /// Decode a manifest from serialized bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let (version, payload) = decode_envelope(
            bytes,
            HEADER_MAGIC,
            FORMAT_VERSION_V1..=FORMAT_VERSION_V3,
            MAX_MANIFEST_PAYLOAD_BYTES,
            SizeCheckMode::ExactMatch,
        )
        .map_err(|err| {
            use htap_common::envelope::EnvelopeError;

            let message = match err {
                EnvelopeError::TooSmall { found, min } => {
                    format!("manifest too short: {found} bytes (minimum {min})")
                }
                EnvelopeError::BadMagic => "invalid manifest magic header".into(),
                EnvelopeError::UnsupportedVersion(version) => {
                    format!("unsupported manifest format version: {version}")
                }
                EnvelopeError::PayloadTooLarge { len, max } => {
                    format!("manifest payload length {len} exceeds maximum {max}")
                }
                EnvelopeError::SizeMismatch { expected, found }
                | EnvelopeError::Truncated { expected, found } => {
                    format!("manifest size mismatch: expected {expected} bytes, got {found}")
                }
                EnvelopeError::TrailingBytes { extra } => {
                    let expected = bytes.len() - extra;
                    format!(
                        "manifest size mismatch: expected {expected} bytes, got {}",
                        bytes.len()
                    )
                }
                EnvelopeError::ChecksumMismatch { expected, actual } => {
                    format!(
                        "manifest CRC mismatch: expected {expected:#010x}, calculated {actual:#010x}"
                    )
                }
            };
            HtapError::Corruption(message)
        })?;

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

        // Each SST entry is at least 18 bytes (8 id + 8 count + 1 min_flag + 1 max_flag).
        // Check cursor length before allocation to guard against memory exhaustion.
        let min_sst_bytes = (sst_count as usize).checked_mul(18).ok_or_else(|| {
            HtapError::Corruption("manifest SST count causes byte overflow".into())
        })?;
        if cursor.len() < min_sst_bytes {
            return Err(HtapError::Corruption(format!(
                "manifest payload too short for SST count {sst_count}: need at least {min_sst_bytes} bytes, have {}",
                cursor.len()
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

        let applied_txns = if version == FORMAT_VERSION_V1 {
            if !cursor.is_empty() {
                return Err(HtapError::Corruption(format!(
                    "manifest v1 has {} trailing leftover bytes",
                    cursor.len()
                )));
            }
            Vec::new()
        } else {
            if cursor.len() < 4 {
                return Err(HtapError::Corruption(
                    "manifest payload too short for external ledger count".into(),
                ));
            }
            let ledger_count = u32::from_le_bytes(cursor[0..4].try_into().unwrap());
            cursor = &cursor[4..];

            if (ledger_count as usize) > MAX_APPLIED_EXTERNAL_TXNS {
                return Err(HtapError::Corruption(format!(
                    "manifest external ledger count {ledger_count} exceeds maximum {MAX_APPLIED_EXTERNAL_TXNS}"
                )));
            }

            let required_bytes = (ledger_count as usize)
                .checked_mul(16)
                .ok_or_else(|| HtapError::Corruption("ledger count causes byte overflow".into()))?;
            if cursor.len() < required_bytes {
                return Err(HtapError::Corruption(format!(
                    "manifest payload too short for external ledger count {ledger_count}: need {required_bytes} bytes, have {}",
                    cursor.len()
                )));
            }

            let mut txns = Vec::with_capacity(ledger_count as usize);
            let mut seen_ids = std::collections::HashSet::with_capacity(ledger_count as usize);

            for _ in 0..ledger_count {
                let txn_id = u64::from_le_bytes(cursor[0..8].try_into().unwrap());
                let ver = u64::from_le_bytes(cursor[8..16].try_into().unwrap());
                cursor = &cursor[16..];

                if txn_id == 0 {
                    return Err(HtapError::Corruption(
                        "manifest ledger entry txn_id cannot be zero".into(),
                    ));
                }
                if !seen_ids.insert(txn_id) {
                    return Err(HtapError::Corruption(format!(
                        "duplicate transaction id {txn_id} in manifest ledger"
                    )));
                }

                txns.push(ManifestLedgerEntry {
                    txn_id,
                    version: Version::new(ver),
                });
            }
            txns
        };

        let derived_high_water = ssts
            .iter()
            .filter_map(|sst| sst.max_version)
            .max()
            .unwrap_or(Version::INITIAL);
        let (committed_version_high_water, gc_low_water) = if version == FORMAT_VERSION_V3 {
            if cursor.len() < 16 {
                return Err(HtapError::Corruption(
                    "manifest payload too short for version watermarks".into(),
                ));
            }
            let committed = Version::new(u64::from_le_bytes(cursor[0..8].try_into().unwrap()));
            let gc = Version::new(u64::from_le_bytes(cursor[8..16].try_into().unwrap()));
            cursor = &cursor[16..];
            (committed, gc)
        } else {
            (derived_high_water, Version::INITIAL)
        };

        if !cursor.is_empty() {
            return Err(HtapError::Corruption(format!(
                "manifest has {} trailing leftover bytes",
                cursor.len()
            )));
        }

        if version == FORMAT_VERSION_V3 {
            if let Some(max_ledger_version) = applied_txns.iter().map(|entry| entry.version).max() {
                if max_ledger_version > committed_version_high_water {
                    return Err(HtapError::Corruption(format!(
                        "manifest ledger version {max_ledger_version} exceeds committed-version high-water {committed_version_high_water}"
                    )));
                }
            }
        }

        Ok(Manifest {
            ssts,
            applied_txns,
            committed_version_high_water,
            gc_low_water,
        })
    }

    /// Read a manifest from a file path. Returns `Ok(None)` if file does not exist.
    pub fn read_from_file(path: &Path) -> Result<Option<Self>> {
        let file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(HtapError::Io(e)),
        };

        let len = file.metadata()?.len();
        if len < HEADER_LEN as u64 {
            return Err(HtapError::Corruption(format!(
                "manifest file too short: {len} bytes (minimum {HEADER_LEN})"
            )));
        }
        let max_allowed = (HEADER_LEN as u64) + (MAX_MANIFEST_PAYLOAD_BYTES as u64);
        let bytes = read_file_exact_bounded_from_file(file, path, max_allowed as usize).map_err(
            |err| match err {
                HtapError::Corruption(message)
                    if message
                        == format!(
                            "file size {len} for '{}' exceeds maximum allowed bound {max_allowed}",
                            path.display()
                        ) =>
                {
                    HtapError::Corruption(format!(
                        "manifest file size {len} exceeds maximum allowed {max_allowed}"
                    ))
                }
                HtapError::Corruption(message)
                    if message
                        == format!(
                            "file '{}' truncated while reading: expected {len} bytes, encountered EOF",
                            path.display()
                        ) =>
                {
                    HtapError::Corruption(format!(
                        "manifest file short read: expected {len} bytes, encountered EOF"
                    ))
                }
                other => other,
            },
        )?;

        let manifest = Self::decode(&bytes)?;
        Ok(Some(manifest))
    }

    /// Atomically publish the manifest: write `dir/MANIFEST.tmp` -> fsync -> rename -> fsync `dir`.
    pub fn atomic_publish(dir: &Path, manifest: &Manifest) -> Result<()> {
        if let Some(current) = Self::read_from_file(&dir.join("MANIFEST"))? {
            if manifest.committed_version_high_water < current.committed_version_high_water {
                return Err(HtapError::InvalidArgument(format!(
                    "manifest committed-version high-water cannot move backward from {} to {}",
                    current.committed_version_high_water, manifest.committed_version_high_water
                )));
            }
            if manifest.gc_low_water < current.gc_low_water {
                return Err(HtapError::InvalidArgument(format!(
                    "manifest GC low-water cannot move backward from {} to {}",
                    current.gc_low_water, manifest.gc_low_water
                )));
            }
        }
        if let Some(max_ledger_version) = manifest
            .applied_txns
            .iter()
            .map(|entry| entry.version)
            .max()
        {
            if max_ledger_version > manifest.committed_version_high_water {
                return Err(HtapError::InvalidArgument(format!(
                    "manifest ledger version {max_ledger_version} exceeds committed-version high-water {}",
                    manifest.committed_version_high_water
                )));
            }
        }

        let encoded = manifest.encode()?;
        atomic_publish(dir, "MANIFEST.tmp", "MANIFEST", &encoded, None, false)
    }
}

impl Default for Manifest {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_manifest_empty_round_trip() {
        let manifest = Manifest::new();
        let bytes = manifest.encode().unwrap();
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

        let bytes = manifest.encode().unwrap();
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
        let mut bytes = manifest.encode().unwrap();
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
        let mut bytes = manifest.encode().unwrap();
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

    #[test]
    fn test_manifest_v3_round_trip_with_ledger() {
        let mut manifest = Manifest::new();
        manifest.prepend(ManifestSstEntry {
            id: 1,
            entry_count: 50,
            min_version: Some(Version::new(2)),
            max_version: Some(Version::new(10)),
        });
        manifest
            .applied_txns
            .push(ManifestLedgerEntry::new(101, Version::new(2)));
        manifest
            .applied_txns
            .push(ManifestLedgerEntry::new(102, Version::new(5)));
        manifest.committed_version_high_water = Version::new(10);

        let bytes = manifest.encode().unwrap();
        // Check that format_version in bytes is 3.
        let version = u16::from_le_bytes(bytes[8..10].try_into().unwrap());
        assert_eq!(version, FORMAT_VERSION_V3);

        let decoded = Manifest::decode(&bytes).unwrap();
        assert_eq!(manifest, decoded);
    }

    #[test]
    fn test_manifest_v1_fixture_decodes_as_empty_ledger() {
        let mut manifest = Manifest::new();
        manifest.prepend(ManifestSstEntry {
            id: 7,
            entry_count: 25,
            min_version: Some(Version::new(3)),
            max_version: Some(Version::new(9)),
        });

        let v1_bytes = manifest.encode_v1();
        let version = u16::from_le_bytes(v1_bytes[8..10].try_into().unwrap());
        assert_eq!(version, FORMAT_VERSION_V1);

        let decoded = Manifest::decode(&v1_bytes).unwrap();
        assert_eq!(decoded.ssts, manifest.ssts);
        assert!(decoded.applied_txns.is_empty());
    }

    #[test]
    fn test_manifest_unknown_version() {
        let manifest = Manifest::new();
        let mut bytes = manifest.encode().unwrap();

        // Version 99 is outside the supported v1-v3 range.
        bytes[8..10].copy_from_slice(&99u16.to_le_bytes());
        let err = Manifest::decode(&bytes).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Corruption error: unsupported manifest format version: 99"
        );

        // Version 0 is also outside the supported range.
        bytes[8..10].copy_from_slice(&0u16.to_le_bytes());
        let err0 = Manifest::decode(&bytes).unwrap_err();
        assert_eq!(
            err0.to_string(),
            "Corruption error: unsupported manifest format version: 0"
        );
    }

    #[test]
    fn test_manifest_oversized_payload_len() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(HEADER_MAGIC);
        bytes.extend_from_slice(&FORMAT_VERSION_V2.to_le_bytes());
        bytes.extend_from_slice(&(MAX_MANIFEST_PAYLOAD_BYTES + 1).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes()); // dummy crc

        let err = Manifest::decode(&bytes).unwrap_err();
        assert!(err.to_string().contains("exceeds maximum"));
    }

    #[test]
    fn test_manifest_oversized_sst_count() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&(MAX_SST_COUNT + 1).to_le_bytes());
        let payload_len = payload.len() as u32;
        let crc = crc32c::crc32c(&payload);

        let mut bytes = Vec::new();
        bytes.extend_from_slice(HEADER_MAGIC);
        bytes.extend_from_slice(&FORMAT_VERSION_V2.to_le_bytes());
        bytes.extend_from_slice(&payload_len.to_le_bytes());
        bytes.extend_from_slice(&crc.to_le_bytes());
        bytes.extend_from_slice(&payload);

        let err = Manifest::decode(&bytes).unwrap_err();
        assert!(err.to_string().contains("SST count"));
    }

    #[test]
    fn test_manifest_oversized_ledger_count() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&0u32.to_le_bytes()); // 0 ssts
        payload.extend_from_slice(&((MAX_APPLIED_EXTERNAL_TXNS as u32) + 1).to_le_bytes());
        let payload_len = payload.len() as u32;
        let crc = crc32c::crc32c(&payload);

        let mut bytes = Vec::new();
        bytes.extend_from_slice(HEADER_MAGIC);
        bytes.extend_from_slice(&FORMAT_VERSION_V2.to_le_bytes());
        bytes.extend_from_slice(&payload_len.to_le_bytes());
        bytes.extend_from_slice(&crc.to_le_bytes());
        bytes.extend_from_slice(&payload);

        let err = Manifest::decode(&bytes).unwrap_err();
        assert!(err.to_string().contains("ledger count"));
    }

    #[test]
    fn test_manifest_ledger_count_exceeds_remaining_payload_no_oom() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&0u32.to_le_bytes()); // 0 ssts
                                                        // Claims 50,000 entries but provides only 4 bytes
        payload.extend_from_slice(&50_000u32.to_le_bytes());
        payload.extend_from_slice(&[1, 2, 3, 4]); // not enough for 50k * 16 bytes
        let payload_len = payload.len() as u32;
        let crc = crc32c::crc32c(&payload);

        let mut bytes = Vec::new();
        bytes.extend_from_slice(HEADER_MAGIC);
        bytes.extend_from_slice(&FORMAT_VERSION_V2.to_le_bytes());
        bytes.extend_from_slice(&payload_len.to_le_bytes());
        bytes.extend_from_slice(&crc.to_le_bytes());
        bytes.extend_from_slice(&payload);

        let err = Manifest::decode(&bytes).unwrap_err();
        assert!(err
            .to_string()
            .contains("too short for external ledger count"));
    }

    #[test]
    fn test_manifest_rejects_zero_txn_id() {
        let mut manifest = Manifest::new();
        manifest
            .applied_txns
            .push(ManifestLedgerEntry::new(0, Version::new(2)));

        let bytes = manifest.encode().unwrap();
        let err = Manifest::decode(&bytes).unwrap_err();
        assert!(err.to_string().contains("txn_id cannot be zero"));
    }

    #[test]
    fn test_manifest_rejects_duplicate_txn_id() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&0u32.to_le_bytes()); // 0 ssts
        payload.extend_from_slice(&2u32.to_le_bytes()); // 2 entries
                                                        // entry 1: txn 42, v2
        payload.extend_from_slice(&42u64.to_le_bytes());
        payload.extend_from_slice(&2u64.to_le_bytes());
        // entry 2: txn 42, v3 (duplicate ID)
        payload.extend_from_slice(&42u64.to_le_bytes());
        payload.extend_from_slice(&3u64.to_le_bytes());

        let payload_len = payload.len() as u32;
        let crc = crc32c::crc32c(&payload);

        let mut bytes = Vec::new();
        bytes.extend_from_slice(HEADER_MAGIC);
        bytes.extend_from_slice(&FORMAT_VERSION_V2.to_le_bytes());
        bytes.extend_from_slice(&payload_len.to_le_bytes());
        bytes.extend_from_slice(&crc.to_le_bytes());
        bytes.extend_from_slice(&payload);

        let err = Manifest::decode(&bytes).unwrap_err();
        assert!(err.to_string().contains("duplicate transaction id 42"));
    }

    #[test]
    fn test_manifest_rejects_trailing_leftover_bytes() {
        let mut manifest = Manifest::new();
        manifest
            .applied_txns
            .push(ManifestLedgerEntry::new(1, Version::new(2)));
        let mut bytes = manifest.encode().unwrap();

        // Append trailing byte and update header payload_len and CRC
        let mut payload = bytes[HEADER_LEN..].to_vec();
        payload.push(0xaa);
        let payload_len = payload.len() as u32;
        let crc = crc32c::crc32c(&payload);
        bytes[10..14].copy_from_slice(&payload_len.to_le_bytes());
        bytes[14..18].copy_from_slice(&crc.to_le_bytes());
        bytes.truncate(HEADER_LEN);
        bytes.extend_from_slice(&payload);

        let err = Manifest::decode(&bytes).unwrap_err();
        assert!(err.to_string().contains("trailing leftover bytes"));
    }

    #[test]
    fn test_manifest_v2_crc_corruption() {
        let mut manifest = Manifest::new();
        manifest
            .applied_txns
            .push(ManifestLedgerEntry::new(1, Version::new(2)));
        let mut bytes = manifest.encode().unwrap();
        let last_idx = bytes.len() - 1;
        bytes[last_idx] ^= 0xff;

        assert!(matches!(
            Manifest::decode(&bytes),
            Err(HtapError::Corruption(_))
        ));
    }

    #[test]
    fn test_manifest_encode_cap_rejection_sst_count() {
        let mut manifest = Manifest::new();
        manifest.ssts = vec![
            ManifestSstEntry {
                id: 1,
                entry_count: 1,
                min_version: None,
                max_version: None,
            };
            (MAX_SST_COUNT as usize) + 1
        ];
        let err = manifest.encode().unwrap_err();
        assert!(matches!(err, HtapError::InvalidArgument(_)));
        assert!(err.to_string().contains("SST count"));
    }

    #[test]
    fn test_manifest_encode_cap_rejection_ledger_count() {
        let mut manifest = Manifest::new();
        manifest.applied_txns =
            vec![ManifestLedgerEntry::new(1, Version::new(1)); MAX_APPLIED_EXTERNAL_TXNS + 1];
        let err = manifest.encode().unwrap_err();
        assert!(matches!(err, HtapError::InvalidArgument(_)));
        assert!(err.to_string().contains("applied_txns"));
    }

    #[test]
    fn test_manifest_read_from_file_oversized_rejected_before_allocation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("MANIFEST");
        let file = std::fs::File::create(&path).unwrap();
        // Set sparse length exceeding HEADER_LEN + MAX_MANIFEST_PAYLOAD_BYTES (e.g. 100 GB)
        file.set_len(100 * 1024 * 1024 * 1024).unwrap();
        drop(file);

        let err = Manifest::read_from_file(&path).unwrap_err();
        assert!(matches!(err, HtapError::Corruption(_)));
        assert!(err.to_string().contains("exceeds maximum allowed"));
    }

    #[test]
    fn test_manifest_read_from_file_short_content_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("MANIFEST");
        std::fs::write(&path, b"short").unwrap();

        let err = Manifest::read_from_file(&path).unwrap_err();
        assert!(matches!(err, HtapError::Corruption(_)));
        assert!(err.to_string().contains("manifest file too short"));
    }

    #[test]
    fn test_manifest_read_from_file_not_found_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("NONEXISTENT_MANIFEST");
        let res = Manifest::read_from_file(&path).unwrap();
        assert_eq!(res, None);
    }

    #[test]
    fn test_manifest_read_from_file_trailing_content_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("MANIFEST");
        let manifest = Manifest::new();
        let mut bytes = manifest.encode().unwrap();
        bytes.push(0xff); // extra trailing byte
        std::fs::write(&path, &bytes).unwrap();

        let err = Manifest::read_from_file(&path).unwrap_err();
        assert!(matches!(err, HtapError::Corruption(_)));
    }

    #[test]
    fn test_manifest_v1_golden_bytes() {
        let manifest = Manifest::new();
        let bytes = manifest.encode_v1();
        assert_eq!(
            bytes,
            [72, 84, 65, 80, 77, 65, 78, 49, 1, 0, 4, 0, 0, 0, 199, 75, 103, 72, 0, 0, 0, 0,]
        );
    }

    #[test]
    fn test_manifest_v3_golden_bytes() {
        let bytes = Manifest::new().encode().unwrap();
        assert_eq!(
            bytes,
            [
                72, 84, 65, 80, 77, 65, 78, 49, 3, 0, 24, 0, 0, 0, 55, 156, 203, 63, 0, 0, 0, 0, 0,
                0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0,
            ]
        );
    }

    #[test]
    fn test_manifest_v1_bad_magic_and_oversized() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"BADMAGIC");
        bytes.extend_from_slice(&FORMAT_VERSION_V1.to_le_bytes());
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());

        let bad_magic = Manifest::decode(&bytes).unwrap_err();
        assert_eq!(
            bad_magic.to_string(),
            "Corruption error: invalid manifest magic header"
        );

        bytes[..8].copy_from_slice(HEADER_MAGIC);
        let oversized = Manifest::decode(&bytes).unwrap_err();
        assert_eq!(
            oversized.to_string(),
            format!(
                "Corruption error: manifest payload length {} exceeds maximum {}",
                u32::MAX,
                MAX_MANIFEST_PAYLOAD_BYTES
            )
        );
    }

    #[test]
    fn test_manifest_v1_bad_version_and_crc() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(HEADER_MAGIC);
        bytes.extend_from_slice(&99u16.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());

        let bad_version = Manifest::decode(&bytes).unwrap_err();
        assert_eq!(
            bad_version.to_string(),
            "Corruption error: unsupported manifest format version: 99"
        );

        bytes[8..10].copy_from_slice(&FORMAT_VERSION_V1.to_le_bytes());
        bytes[10..14].copy_from_slice(&4u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());

        let bad_crc = Manifest::decode(&bytes).unwrap_err();
        assert_eq!(
            bad_crc.to_string(),
            format!(
                "Corruption error: manifest CRC mismatch: expected {:#010x}, calculated {:#010x}",
                0,
                crc32c::crc32c(&0u32.to_le_bytes())
            )
        );
    }

    #[test]
    fn test_manifest_v1_size_check_truncated() {
        let payload = 0u32.to_le_bytes();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(HEADER_MAGIC);
        bytes.extend_from_slice(&FORMAT_VERSION_V1.to_le_bytes());
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&crc32c::crc32c(&payload).to_le_bytes());
        bytes.extend_from_slice(&payload[..payload.len() - 1]);

        let err = Manifest::decode(&bytes).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Corruption error: manifest size mismatch: expected 22 bytes, got 21"
        );
    }

    #[test]
    fn test_manifest_v1_size_check_trailing() {
        let payload = 0u32.to_le_bytes();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(HEADER_MAGIC);
        bytes.extend_from_slice(&FORMAT_VERSION_V1.to_le_bytes());
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&crc32c::crc32c(&payload).to_le_bytes());
        bytes.extend_from_slice(&payload);
        bytes.push(0);

        let err = Manifest::decode(&bytes).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Corruption error: manifest size mismatch: expected 22 bytes, got 23"
        );
    }

    #[test]
    fn test_manifest_read_from_file_reports_trailing_probe_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("MANIFEST");
        let mut bytes = Manifest::new().encode().unwrap();
        bytes.push(0xff);
        std::fs::write(&path, bytes).unwrap();

        let err = Manifest::read_from_file(&path).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Corruption error: manifest size mismatch: expected 42 bytes, got 43"
        );
    }
}
