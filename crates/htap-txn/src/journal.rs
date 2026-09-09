//! CRC32C-framed durable Intent/Commit journal with bounded frames,
//! torn-final repair, and corruption validation.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use htap_common::{HtapError, Result, Version};
use serde::{Deserialize, Serialize};

use crate::participant::{ParticipantWork, TransactionId};

/// Size of fixed header: `payload_len: u32 LE` (4 bytes) + `crc32c: u32 LE` (4 bytes).
pub const HEADER_SIZE: usize = 8;

/// Default maximum frame payload size (16 MiB). Prevents unbounded allocation
/// if corrupted length headers are encountered.
pub const DEFAULT_MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

/// Entries logged in the transaction journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum JournalRecord {
    /// Two-phase commit Intent: records staged participant involvement, snapshot version, and exact payloads.
    Intent {
        txn_id: TransactionId,
        snapshot: Version,
        participants: Vec<ParticipantWork>,
    },
    /// Two-phase commit linearization point: transaction is durable and committed.
    Commit {
        txn_id: TransactionId,
        version: Version,
    },
    /// Transaction abort record: uncommitted or explicitly cancelled.
    Abort { txn_id: TransactionId },
}

impl JournalRecord {
    /// Return the transaction ID for this journal record.
    pub fn txn_id(&self) -> TransactionId {
        match self {
            Self::Intent { txn_id, .. } | Self::Commit { txn_id, .. } | Self::Abort { txn_id } => {
                *txn_id
            }
        }
    }

    /// Return the commit version pinned by this record, if Commit.
    pub fn version(&self) -> Option<Version> {
        match self {
            Self::Commit { version, .. } => Some(*version),
            Self::Intent { .. } | Self::Abort { .. } => None,
        }
    }

    /// Return the snapshot version recorded in this record, if Intent.
    pub fn snapshot(&self) -> Option<Version> {
        match self {
            Self::Intent { snapshot, .. } => Some(*snapshot),
            Self::Commit { .. } | Self::Abort { .. } => None,
        }
    }

    /// Return a human-readable record kind name.
    pub fn record_type(&self) -> &'static str {
        match self {
            Self::Intent { .. } => "Intent",
            Self::Commit { .. } => "Commit",
            Self::Abort { .. } => "Abort",
        }
    }
}

/// Options for configuring and opening a [`Journal`].
#[derive(Debug, Clone)]
pub struct JournalOptions {
    /// Filesystem path to the journal file.
    pub path: PathBuf,
    /// Upper bound for an individual frame's payload size in bytes.
    pub max_frame_size: usize,
    /// Whether appends fsync immediately. Default is true for synchronous durability.
    pub sync_on_write: bool,
    /// Automatically truncate torn partial frames at EOF upon opening.
    pub auto_repair_torn_final: bool,
}

impl JournalOptions {
    /// Creates default options for a journal at the specified path.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            max_frame_size: DEFAULT_MAX_FRAME_SIZE,
            sync_on_write: true,
            auto_repair_torn_final: true,
        }
    }

    /// Override the maximum allowed frame payload size.
    #[must_use]
    pub fn with_max_frame_size(mut self, size: usize) -> Self {
        self.max_frame_size = size;
        self
    }

    /// Override immediate fsync behavior.
    #[must_use]
    pub fn with_sync_on_write(mut self, sync: bool) -> Self {
        self.sync_on_write = sync;
        self
    }

    /// Override whether to automatically repair torn final records on open.
    #[must_use]
    pub fn with_auto_repair(mut self, repair: bool) -> Self {
        self.auto_repair_torn_final = repair;
        self
    }
}

/// Outcome of scanning the journal.
#[derive(Debug, Clone)]
pub struct JournalScan {
    /// Intact valid records with their starting byte offsets.
    pub records: Vec<(u64, JournalRecord)>,
    /// Byte offset immediately after the last valid record.
    pub valid_end: u64,
    /// File size in bytes at scan time.
    pub file_len: u64,
    /// Details if a torn record was encountered at EOF.
    pub torn_final: Option<(u64, String)>,
    /// Details if corrupt record / CRC mismatch was encountered before EOF.
    pub middle_corrupt: Option<(u64, String)>,
}

/// Serializes and frames a [`JournalRecord`] with a 4-byte payload length and 4-byte CRC32C checksum.
pub fn encode_frame(record: &JournalRecord, max_frame_size: usize) -> Result<Vec<u8>> {
    let payload = serde_json::to_vec(record)
        .map_err(|e| HtapError::Internal(format!("failed to serialize journal record: {e}")))?;

    if payload.is_empty() {
        return Err(HtapError::Corruption(
            "serialized journal record payload cannot be empty".into(),
        ));
    }

    if payload.len() > max_frame_size {
        return Err(HtapError::InvalidArgument(format!(
            "journal record size {} exceeds max frame size {max_frame_size}",
            payload.len()
        )));
    }

    let crc = crc32c::crc32c(&payload);
    let mut frame = Vec::with_capacity(HEADER_SIZE + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&crc.to_le_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// Internal status when decoding an individual frame from bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameStatus {
    /// Clean end of file.
    CleanEof,
    /// Valid record with total frame length (header + payload).
    Valid {
        record: JournalRecord,
        frame_len: usize,
    },
    /// Torn tail at EOF that can be repaired by truncation.
    TornFinal { offset: u64, reason: String },
    /// Corruption in the middle of the log or unrecoverable frame.
    Corrupt { offset: u64, reason: String },
}

/// Decodes one frame from a byte buffer at the given offset.
pub fn decode_frame_slice(
    data: &[u8],
    offset: u64,
    total_len: u64,
    max_frame_size: usize,
) -> FrameStatus {
    let remaining = data.len();
    if remaining == 0 {
        return FrameStatus::CleanEof;
    }

    if remaining < HEADER_SIZE {
        return FrameStatus::TornFinal {
            offset,
            reason: format!(
                "incomplete header: only {remaining} bytes remaining, expected {HEADER_SIZE}"
            ),
        };
    }

    let payload_len = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let expected_crc = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);

    if payload_len == 0 {
        if offset + (remaining as u64) == total_len {
            return FrameStatus::TornFinal {
                offset,
                reason: "zero payload length at end of file".into(),
            };
        }
        return FrameStatus::Corrupt {
            offset,
            reason: "zero payload length inside journal".into(),
        };
    }

    if payload_len > max_frame_size {
        if offset + (HEADER_SIZE as u64) == total_len {
            return FrameStatus::TornFinal {
                offset,
                reason: format!("unbounded payload length {payload_len} at EOF"),
            };
        }
        return FrameStatus::Corrupt {
            offset,
            reason: format!(
                "frame payload length {payload_len} exceeds bounded max {max_frame_size}"
            ),
        };
    }

    let expected_frame_len = HEADER_SIZE + payload_len;
    if remaining < expected_frame_len {
        if has_valid_frame_ahead(&data[1..], max_frame_size) {
            return FrameStatus::Corrupt {
                offset,
                reason: format!(
                    "corrupted frame at offset {offset}: claimed length {payload_len} exceeds remaining bytes, but subsequent valid frames exist"
                ),
            };
        }

        return FrameStatus::TornFinal {
            offset,
            reason: format!(
                "truncated frame payload: need {payload_len} bytes, only {} remaining",
                remaining - HEADER_SIZE
            ),
        };
    }

    let payload = &data[HEADER_SIZE..expected_frame_len];
    let actual_crc = crc32c::crc32c(payload);

    let is_eof_record = offset + (expected_frame_len as u64) == total_len;

    if actual_crc != expected_crc {
        if is_eof_record {
            return FrameStatus::TornFinal {
                offset,
                reason: format!(
                    "CRC32C mismatch at final record: expected {expected_crc:#010x}, calculated {actual_crc:#010x}"
                ),
            };
        }
        return FrameStatus::Corrupt {
            offset,
            reason: format!(
                "CRC32C mismatch in journal: expected {expected_crc:#010x}, calculated {actual_crc:#010x}"
            ),
        };
    }

    let record: JournalRecord = match serde_json::from_slice(payload) {
        Ok(rec) => rec,
        Err(e) => {
            if is_eof_record {
                return FrameStatus::TornFinal {
                    offset,
                    reason: format!("deserialization failure at final record: {e}"),
                };
            }
            return FrameStatus::Corrupt {
                offset,
                reason: format!("deserialization failure at offset {offset}: {e}"),
            };
        }
    };

    FrameStatus::Valid {
        record,
        frame_len: expected_frame_len,
    }
}

/// Probes ahead to determine if a valid CRC-verified frame exists in the remaining byte slice.
fn has_valid_frame_ahead(data: &[u8], max_frame_size: usize) -> bool {
    for i in 0..data.len() {
        let sub = &data[i..];
        if sub.len() < HEADER_SIZE {
            break;
        }
        let p_len = u32::from_le_bytes([sub[0], sub[1], sub[2], sub[3]]) as usize;
        let exp_crc = u32::from_le_bytes([sub[4], sub[5], sub[6], sub[7]]);
        if p_len > 0 && p_len <= max_frame_size && sub.len() >= HEADER_SIZE + p_len {
            let payload = &sub[HEADER_SIZE..HEADER_SIZE + p_len];
            if crc32c::crc32c(payload) == exp_crc
                && serde_json::from_slice::<JournalRecord>(payload).is_ok()
            {
                return true;
            }
        }
    }
    false
}

/// Durable, synchronous transaction journal.
#[derive(Debug)]
pub struct Journal {
    opts: JournalOptions,
    file: File,
    valid_end: u64,
}

impl Journal {
    /// Open or create a journal file at `path` using default options.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        Self::open_with_options(JournalOptions::new(path))
    }

    /// Open or create a journal file with specific configuration options.
    pub fn open_with_options(opts: JournalOptions) -> Result<Self> {
        if let Some(parent) = opts.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
                if let Ok(dir) = File::open(parent) {
                    let _ = dir.sync_all();
                }
            }
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&opts.path)?;

        let mut journal = Self {
            opts,
            file,
            valid_end: 0,
        };

        if journal.opts.auto_repair_torn_final {
            journal.repair_torn_final()?;
        } else {
            let scan = journal.scan()?;
            if let Some((_, reason)) = scan.middle_corrupt {
                return Err(HtapError::Corruption(reason));
            }
            journal.valid_end = scan.valid_end;
        }

        journal.file.seek(SeekFrom::Start(journal.valid_end))?;
        Ok(journal)
    }

    /// Scans the entire journal, validating CRC32C checksums and frame bounds.
    pub fn scan(&mut self) -> Result<JournalScan> {
        self.file.seek(SeekFrom::Start(0))?;
        let mut buffer = Vec::new();
        self.file.read_to_end(&mut buffer)?;
        let file_len = buffer.len() as u64;

        let mut records = Vec::new();
        let mut pos: u64 = 0;
        let mut torn_final = None;
        let mut middle_corrupt = None;

        while (pos as usize) < buffer.len() {
            let slice = &buffer[pos as usize..];
            match decode_frame_slice(slice, pos, file_len, self.opts.max_frame_size) {
                FrameStatus::CleanEof => break,
                FrameStatus::Valid { record, frame_len } => {
                    records.push((pos, record));
                    pos += frame_len as u64;
                }
                FrameStatus::TornFinal { offset, reason } => {
                    torn_final = Some((offset, reason));
                    break;
                }
                FrameStatus::Corrupt { offset, reason } => {
                    middle_corrupt = Some((offset, reason));
                    break;
                }
            }
        }

        Ok(JournalScan {
            records,
            valid_end: pos,
            file_len,
            torn_final,
            middle_corrupt,
        })
    }

    /// Truncates any torn tail at EOF back to the last valid record boundary.
    ///
    /// Returns the number of bytes truncated. Returns an error if unrecoverable
    /// middle-of-log corruption is found.
    pub fn repair_torn_final(&mut self) -> Result<u64> {
        let scan = self.scan()?;
        if let Some((_, reason)) = scan.middle_corrupt {
            return Err(HtapError::Corruption(format!(
                "cannot repair torn-final on journal with middle corruption: {reason}"
            )));
        }

        let bytes_truncated = scan.file_len.saturating_sub(scan.valid_end);
        if bytes_truncated > 0 {
            tracing::warn!(
                path = %self.opts.path.display(),
                bytes_truncated,
                valid_end = scan.valid_end,
                "repaired torn final record in journal"
            );
            self.file.set_len(scan.valid_end)?;
            self.file.sync_all()?;
        }

        self.valid_end = scan.valid_end;
        self.file.seek(SeekFrom::Start(self.valid_end))?;
        Ok(bytes_truncated)
    }

    /// Checks full integrity of the journal. Returns an error if any frame or CRC is corrupted.
    pub fn check_integrity(&mut self) -> Result<()> {
        let scan = self.scan()?;
        if let Some((offset, reason)) = scan.middle_corrupt {
            return Err(HtapError::Corruption(format!(
                "journal corruption at offset {offset}: {reason}"
            )));
        }
        if let Some((offset, reason)) = scan.torn_final {
            return Err(HtapError::Corruption(format!(
                "journal torn write at offset {offset}: {reason}"
            )));
        }
        Ok(())
    }

    /// Append a [`JournalRecord`] to the journal.
    ///
    /// Synchronously writes the frame to disk. If `sync_on_write` is enabled (the default),
    /// also invokes `fsync`. Returns the byte offset at which the record was written.
    pub fn append(&mut self, record: &JournalRecord) -> Result<u64> {
        let frame = encode_frame(record, self.opts.max_frame_size)?;
        let write_offset = self.valid_end;

        self.file.seek(SeekFrom::Start(write_offset))?;
        self.file.write_all(&frame)?;

        if self.opts.sync_on_write {
            self.file.sync_all()?;
        }

        self.valid_end = write_offset + frame.len() as u64;
        Ok(write_offset)
    }

    /// Explicitly fsync the journal file.
    pub fn sync(&mut self) -> Result<()> {
        self.file.sync_all()?;
        Ok(())
    }

    /// Read all valid records in the journal.
    pub fn read_all(&mut self) -> Result<Vec<JournalRecord>> {
        let scan = self.scan()?;
        if let Some((_, reason)) = scan.middle_corrupt {
            return Err(HtapError::Corruption(reason));
        }
        Ok(scan.records.into_iter().map(|(_, rec)| rec).collect())
    }

    /// Return the byte length of the valid portion of the journal.
    pub fn valid_bytes(&self) -> u64 {
        self.valid_end
    }

    /// Return the configured path of this journal.
    pub fn path(&self) -> &Path {
        &self.opts.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_encode_and_decode_records() {
        let rec1 = JournalRecord::Intent {
            txn_id: TransactionId::new(1),
            snapshot: Version::new(2),
            participants: vec![
                ParticipantWork::new(10, vec![1, 2, 3]),
                ParticipantWork::new(20, vec![4, 5, 6]),
            ],
        };
        let rec2 = JournalRecord::Commit {
            txn_id: TransactionId::new(1),
            version: Version::new(2),
        };
        let rec3 = JournalRecord::Abort {
            txn_id: TransactionId::new(2),
        };

        let frame1 = encode_frame(&rec1, DEFAULT_MAX_FRAME_SIZE).unwrap();
        let frame2 = encode_frame(&rec2, DEFAULT_MAX_FRAME_SIZE).unwrap();
        let frame3 = encode_frame(&rec3, DEFAULT_MAX_FRAME_SIZE).unwrap();

        let mut combined = Vec::new();
        combined.extend_from_slice(&frame1);
        combined.extend_from_slice(&frame2);
        combined.extend_from_slice(&frame3);

        let total = combined.len() as u64;

        let status1 = decode_frame_slice(&combined, 0, total, DEFAULT_MAX_FRAME_SIZE);
        let len1 = match status1 {
            FrameStatus::Valid { record, frame_len } => {
                assert_eq!(record, rec1);
                frame_len
            }
            other => panic!("expected valid frame 1, got {other:?}"),
        };

        let status2 = decode_frame_slice(
            &combined[len1..],
            len1 as u64,
            total,
            DEFAULT_MAX_FRAME_SIZE,
        );
        let len2 = match status2 {
            FrameStatus::Valid { record, frame_len } => {
                assert_eq!(record, rec2);
                frame_len
            }
            other => panic!("expected valid frame 2, got {other:?}"),
        };

        let status3 = decode_frame_slice(
            &combined[len1 + len2..],
            (len1 + len2) as u64,
            total,
            DEFAULT_MAX_FRAME_SIZE,
        );
        match status3 {
            FrameStatus::Valid { record, .. } => {
                assert_eq!(record, rec3);
            }
            other => panic!("expected valid frame 3, got {other:?}"),
        }
    }

    #[test]
    fn test_bounded_frames() {
        let rec = JournalRecord::Intent {
            txn_id: TransactionId::new(10),
            snapshot: Version::new(2),
            participants: vec![ParticipantWork::new(1, vec![42; 500])],
        };

        // Frame encoding respects max_frame_size
        let err = encode_frame(&rec, 100).unwrap_err();
        assert!(matches!(err, HtapError::InvalidArgument(_)));

        // Frame decoding respects max_frame_size
        let valid_frame = encode_frame(&rec, DEFAULT_MAX_FRAME_SIZE).unwrap();
        let status = decode_frame_slice(&valid_frame, 0, valid_frame.len() as u64, 100);
        assert!(matches!(status, FrameStatus::Corrupt { .. }));
    }

    #[test]
    fn test_torn_final_repair() {
        let temp = NamedTempFile::new().unwrap();
        let path = temp.path().to_path_buf();

        let mut journal = Journal::open(&path).unwrap();
        journal
            .append(&JournalRecord::Commit {
                txn_id: TransactionId::new(100),
                version: Version::new(5),
            })
            .unwrap();
        let valid_end = journal.valid_bytes();

        // Simulate torn tail: append partial bytes (less than header)
        {
            let mut file = OpenOptions::new().append(true).open(&path).unwrap();
            file.write_all(&[0xaa, 0xbb, 0xcc]).unwrap();
            file.sync_all().unwrap();
        }

        // Auto repair upon opening
        let mut journal_reopened = Journal::open(&path).unwrap();
        assert_eq!(journal_reopened.valid_bytes(), valid_end);

        let records = journal_reopened.read_all().unwrap();
        assert_eq!(records.len(), 1);

        // Can append new record cleanly after repaired torn tail
        journal_reopened
            .append(&JournalRecord::Abort {
                txn_id: TransactionId::new(101),
            })
            .unwrap();

        let records_after = journal_reopened.read_all().unwrap();
        assert_eq!(records_after.len(), 2);
    }

    #[test]
    fn test_middle_corruption_detected() {
        let temp = NamedTempFile::new().unwrap();
        let path = temp.path().to_path_buf();

        let mut journal = Journal::open(&path).unwrap();
        journal
            .append(&JournalRecord::Commit {
                txn_id: TransactionId::new(1),
                version: Version::new(2),
            })
            .unwrap();
        journal
            .append(&JournalRecord::Commit {
                txn_id: TransactionId::new(2),
                version: Version::new(3),
            })
            .unwrap();
        journal
            .append(&JournalRecord::Commit {
                txn_id: TransactionId::new(3),
                version: Version::new(4),
            })
            .unwrap();

        // Corrupt record 2 in the middle of the file
        let mut bytes = std::fs::read(&path).unwrap();
        // Byte flip somewhere in the middle record
        bytes[45] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();

        // Reopening with auto-repair should refuse to truncate middle corruption
        let res = Journal::open_with_options(JournalOptions::new(&path).with_auto_repair(true));
        assert!(res.is_err());
    }
}
