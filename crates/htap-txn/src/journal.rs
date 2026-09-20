//! CRC32C-framed durable Intent/Commit journal with bounded frames,
//! torn-final repair, and corruption validation.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use htap_common::bytecursor::ByteReader;
use htap_common::{HtapError, Result, Version};
use serde::{Deserialize, Serialize};

use crate::participant::{ParticipantWork, TransactionId};

/// Size of fixed header: `payload_len: u32 LE` (4 bytes) + `crc32c: u32 LE` (4 bytes).
pub const HEADER_SIZE: usize = 8;

/// Default maximum frame payload size (16 MiB). Prevents unbounded allocation
/// if corrupted length headers are encountered.
pub const DEFAULT_MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

/// Default maximum total journal size (64 MiB) to reject oversized journals before allocation.
pub const DEFAULT_MAX_JOURNAL_SIZE: u64 = 64 * 1024 * 1024;

/// Conservative fixed overhead, in bytes, added by [`intent_frame_size_bound`] on top of a
/// payload's own JSON-number-array encoding, covering everything in an `Intent` journal frame
/// that isn't already accounted for per-participant by
/// [`INTENT_PER_PARTICIPANT_OVERHEAD_BYTES`]: the `{"Intent":{"txn_id":...,"snapshot":...,
/// "participants":[...]}}` field names and punctuation, up-to-20-digit `u64` `txn_id`/`snapshot`
/// fields, and the 8-byte length+CRC32C frame header. Rounded well above what a request actually
/// needs.
const INTENT_FRAME_FIXED_OVERHEAD_BYTES: usize = 512;

/// Conservative per-participant overhead, in bytes, added by [`intent_frame_size_bound`] for each
/// participant in the transaction, covering one `{"participant_id":...,"payload":...},` object's
/// own field names, punctuation, and up-to-20-digit `u64` `participant_id` — everything about that
/// object except its `payload` array's own bytes, which are covered separately by the per-byte
/// terms. Storage-reviewer fix-pass round 3, item 3(c): [`INTENT_FRAME_FIXED_OVERHEAD_BYTES`]
/// alone assumed (and was only ever conservative for) a single participant; a transaction with
/// many small-payload participants can still produce a frame whose total per-participant
/// punctuation exceeds a single fixed constant. 128 bytes is well above one such object's actual
/// encoded overhead (`{"participant_id":18446744073709551615,"payload":[]},` is under 60 bytes).
const INTENT_PER_PARTICIPANT_OVERHEAD_BYTES: usize = 128;

/// Conservative upper bound, in bytes, on the encoded size of the durable `Intent` journal frame
/// that would result from a transaction with `num_participants` participants whose mutation
/// payloads (the JSON-encoded `Vec<Mutation>` bytes produced by
/// `RowstoreParticipant::encode_payload`) sum to `total_payload_len_json` bytes.
///
/// `serde_json`'s default `Vec<u8>` encoding writes each byte as a *decimal number* inside a JSON
/// array (`[145,10,...]`), not a compact byte string, so `total_payload_len_json` payload bytes
/// contribute at most `total_payload_len_json * 3` digit bytes (every byte value 0-255 needs at
/// most 3 decimal digits) plus `total_payload_len_json - 1` separating commas plus 2 brackets —
/// this is a conservative (never-underestimating) bound, not an exact one: real payload bytes are
/// typically printable-ASCII JSON text (values in roughly 9-125), mostly 2 digits, so the true
/// encoded size is usually noticeably smaller. This is why a write set that stays within
/// [`crate::MAX_PAYLOAD_SIZE`] (16 MiB of raw mutation JSON) can still produce an `Intent` frame
/// bigger than [`DEFAULT_MAX_FRAME_SIZE`] — storage-reviewer fix-pass item 3: this bound lets a
/// caller reject that case before doing any prepare or journal work, rather than discovering it
/// only when [`encode_frame`] itself fails. `num_participants` scales
/// [`INTENT_PER_PARTICIPANT_OVERHEAD_BYTES`] (fix-pass round 3, item 3(c)) so the bound stays
/// conservative for a transaction with many participants, not just a single one.
pub fn intent_frame_size_bound(total_payload_len_json: usize, num_participants: usize) -> usize {
    let array_digits = total_payload_len_json.saturating_mul(3);
    let commas = total_payload_len_json.saturating_sub(1);
    let brackets = 2usize;
    let per_participant_overhead =
        num_participants.saturating_mul(INTENT_PER_PARTICIPANT_OVERHEAD_BYTES);
    array_digits
        .saturating_add(commas)
        .saturating_add(brackets)
        .saturating_add(INTENT_FRAME_FIXED_OVERHEAD_BYTES)
        .saturating_add(per_participant_overhead)
}

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
    /// Upper bound for total journal file size in bytes before rejecting as oversized corruption.
    /// Default is 64 MiB ([`DEFAULT_MAX_JOURNAL_SIZE`]).
    pub max_journal_size: u64,
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
            max_journal_size: DEFAULT_MAX_JOURNAL_SIZE,
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

    /// Override the maximum allowed total journal size in bytes.
    #[must_use]
    pub fn with_max_journal_size(mut self, size: u64) -> Self {
        self.max_journal_size = size;
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

    Ok(htap_common::envelope::encode_bare_frame(&payload))
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

    let mut header = ByteReader::new(&data[..HEADER_SIZE]);
    let payload_len = header.read_u32_le().expect("length-checked header") as usize;
    let expected_crc = header.read_u32_le().expect("length-checked header");

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
    /// Set once an append or sync leaves this journal handle's on-disk state unknowable or
    /// untrustworthy (storage-reviewer fix-pass round 3, item 1): either a write/sync failed and
    /// the best-effort truncate back to the last known-good boundary also failed (so bytes may
    /// remain on disk past `valid_end`), or a `fsync` failed at all (so the kernel's page-cache
    /// state for whatever was just written is unknowable, even if a later `fsync` on the same fd
    /// would report success). While `Some`, every [`Self::append`]/[`Self::append_nosync`]/
    /// [`Self::sync`] call is rejected; only a fresh [`Self::open`]/[`Self::open_with_options`]
    /// (a brand new file handle and scan, exactly what a real process restart would do) can clear
    /// it.
    poisoned: Option<String>,
    /// Test-only one-shot fault: when `Some(n)`, the next [`Journal::append_nosync`] writes only
    /// the first `n` bytes of its frame and then reports a simulated I/O error, exactly as a real
    /// crashed/short `write_all` would, so tests can exercise the truncate-on-failure recovery
    /// path (fix-pass item 6a) deterministically without needing to defeat the real OS. Consumed
    /// (reset to `None`) on the next `append_nosync` call whether or not it actually fires.
    pending_partial_write_fault_for_test: Option<usize>,
    /// Test-only one-shot fault: when `true`, the next internal `fsync` (from [`Self::append`]'s
    /// own sync-on-write step or from [`Self::sync`]) fails with a simulated I/O error instead of
    /// actually calling `fsync`, exactly as a real failed `fsync` would after its frame's bytes
    /// were already fully `write_all`'d — so tests can exercise the truncate-and-poison recovery
    /// path (fix-pass round 3, item 1(i)) deterministically. Consumed (reset to `false`) the next
    /// time it fires.
    pending_sync_fault_for_test: bool,
    /// Test-only one-shot fault: when `true`, the next best-effort truncate inside
    /// [`Self::truncate_partial_write_best_effort`] fails with a simulated I/O error instead of
    /// actually calling `set_len`, so tests can exercise the poison-on-unrecoverable-truncate-
    /// failure path (fix-pass round 3, item 1(ii)) deterministically. Consumed (reset to `false`)
    /// the next time it fires.
    pending_truncate_fault_for_test: bool,
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

        let metadata = file.metadata()?;
        let file_len = metadata.len();
        if file_len > opts.max_journal_size {
            return Err(HtapError::Corruption(format!(
                "journal file size {file_len} exceeds maximum allowed size {}",
                opts.max_journal_size
            )));
        }

        let mut journal = Self {
            opts,
            file,
            valid_end: 0,
            poisoned: None,
            pending_partial_write_fault_for_test: None,
            pending_sync_fault_for_test: false,
            pending_truncate_fault_for_test: false,
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
        let file_len = self.file.metadata()?.len();
        if file_len > self.opts.max_journal_size {
            return Err(HtapError::Corruption(format!(
                "journal file size {file_len} exceeds maximum allowed size {}",
                self.opts.max_journal_size
            )));
        }

        self.file.seek(SeekFrom::Start(0))?;

        let mut records = Vec::new();
        let mut pos: u64 = 0;
        let mut torn_final = None;
        let mut middle_corrupt = None;

        while pos < file_len {
            let remaining = file_len - pos;
            if remaining < HEADER_SIZE as u64 {
                torn_final = Some((
                    pos,
                    format!(
                        "incomplete header: only {remaining} bytes remaining, expected {HEADER_SIZE}"
                    ),
                ));
                break;
            }

            let mut header = [0u8; HEADER_SIZE];
            self.file.read_exact(&mut header)?;

            let payload_len =
                u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
            let expected_crc = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);

            if payload_len == 0 {
                if pos + HEADER_SIZE as u64 == file_len {
                    torn_final = Some((pos, "zero payload length at end of file".into()));
                } else {
                    middle_corrupt = Some((pos, "zero payload length inside journal".into()));
                }
                break;
            }

            if payload_len > self.opts.max_frame_size {
                if pos + HEADER_SIZE as u64 == file_len {
                    torn_final = Some((
                        pos,
                        format!("unbounded payload length {payload_len} at EOF"),
                    ));
                } else {
                    middle_corrupt = Some((
                        pos,
                        format!(
                            "frame payload length {payload_len} exceeds bounded max {}",
                            self.opts.max_frame_size
                        ),
                    ));
                }
                break;
            }

            let expected_frame_len = (HEADER_SIZE + payload_len) as u64;
            if remaining < expected_frame_len {
                let probe_res = self.has_valid_frame_ahead_stream(pos + 1, file_len);
                let _ = self.file.seek(SeekFrom::Start(self.valid_end));
                let has_ahead = probe_res?;

                if has_ahead {
                    middle_corrupt = Some((
                        pos,
                        format!(
                            "corrupted frame at offset {pos}: claimed length {payload_len} exceeds remaining bytes, but subsequent valid frames exist"
                        ),
                    ));
                } else {
                    torn_final = Some((
                        pos,
                        format!(
                            "truncated frame payload: need {payload_len} bytes, only {} remaining",
                            remaining - HEADER_SIZE as u64
                        ),
                    ));
                }
                break;
            }

            let mut payload = vec![0u8; payload_len];
            self.file.read_exact(&mut payload)?;

            let actual_crc = crc32c::crc32c(&payload);
            let is_eof_record = pos + expected_frame_len == file_len;

            if actual_crc != expected_crc {
                if is_eof_record {
                    torn_final = Some((
                        pos,
                        format!(
                            "CRC32C mismatch at final record: expected {expected_crc:#010x}, calculated {actual_crc:#010x}"
                        ),
                    ));
                } else {
                    middle_corrupt = Some((
                        pos,
                        format!(
                            "CRC32C mismatch in journal: expected {expected_crc:#010x}, calculated {actual_crc:#010x}"
                        ),
                    ));
                }
                break;
            }

            match serde_json::from_slice::<JournalRecord>(&payload) {
                Ok(record) => {
                    records.push((pos, record));
                    pos += expected_frame_len;
                }
                Err(e) => {
                    if is_eof_record {
                        torn_final =
                            Some((pos, format!("deserialization failure at final record: {e}")));
                    } else {
                        middle_corrupt =
                            Some((pos, format!("deserialization failure at offset {pos}: {e}")));
                    }
                    break;
                }
            }
        }

        let _ = self.file.seek(SeekFrom::Start(self.valid_end));

        Ok(JournalScan {
            records,
            valid_end: pos,
            file_len,
            torn_final,
            middle_corrupt,
        })
    }

    /// Streaming probe starting at `start_pos` up to `file_len` to determine if a valid
    /// frame exists ahead in the journal, using fixed-size header and payload allocations
    /// bounded by `max_frame_size`, without allocating proportional to remaining journal size.
    fn has_valid_frame_ahead_stream(&mut self, start_pos: u64, file_len: u64) -> Result<bool> {
        const CHUNK_SIZE: usize = 8192;
        let mut chunk = [0u8; CHUNK_SIZE];
        let mut chunk_start = start_pos;
        let mut chunk_len = 0usize;

        let mut payload_buf = Vec::new();
        let mut probe_pos = start_pos;

        while probe_pos + (HEADER_SIZE as u64) <= file_len {
            if probe_pos < chunk_start
                || probe_pos + (HEADER_SIZE as u64) > chunk_start + (chunk_len as u64)
            {
                self.file.seek(SeekFrom::Start(probe_pos))?;
                let to_read = (file_len - probe_pos).min(CHUNK_SIZE as u64) as usize;
                self.file.read_exact(&mut chunk[..to_read])?;
                chunk_start = probe_pos;
                chunk_len = to_read;
            }

            let offset_in_chunk = (probe_pos - chunk_start) as usize;
            let header = &chunk[offset_in_chunk..offset_in_chunk + HEADER_SIZE];
            let payload_len =
                u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
            let expected_crc = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);

            if payload_len > 0
                && payload_len <= self.opts.max_frame_size
                && probe_pos + (HEADER_SIZE + payload_len) as u64 <= file_len
            {
                let payload_start = probe_pos + HEADER_SIZE as u64;
                let payload_end = payload_start + payload_len as u64;

                let valid = if payload_end <= chunk_start + (chunk_len as u64) {
                    let p_offset = (payload_start - chunk_start) as usize;
                    let payload = &chunk[p_offset..p_offset + payload_len];
                    crc32c::crc32c(payload) == expected_crc
                        && serde_json::from_slice::<JournalRecord>(payload).is_ok()
                } else {
                    payload_buf.resize(payload_len, 0);
                    self.file.seek(SeekFrom::Start(payload_start))?;
                    self.file.read_exact(&mut payload_buf)?;
                    chunk_len = 0;
                    crc32c::crc32c(&payload_buf) == expected_crc
                        && serde_json::from_slice::<JournalRecord>(&payload_buf).is_ok()
                };

                if valid {
                    return Ok(true);
                }
            }

            probe_pos += 1;
        }

        Ok(false)
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

    /// Append a [`JournalRecord`] without fsyncing.
    ///
    /// Writes the framed record to disk and updates `valid_end`. Durability requires
    /// calling [`Journal::sync`].
    pub fn append_nosync(&mut self, record: &JournalRecord) -> Result<u64> {
        self.check_poisoned()?;
        let frame = encode_frame(record, self.opts.max_frame_size)?;
        let write_offset = self.valid_end;

        self.file.seek(SeekFrom::Start(write_offset))?;

        if let Some(partial_len) = self.pending_partial_write_fault_for_test.take() {
            let partial_len = partial_len.min(frame.len());
            let _ = self.file.write_all(&frame[..partial_len]);
            if let Err(truncate_err) = self.truncate_partial_write_best_effort(write_offset) {
                self.poison(format!(
                    "simulated partial write fault (test injection) at offset {write_offset} \
                     left partial bytes on disk, and the best-effort truncate back to the last \
                     known-good boundary also failed: {truncate_err}"
                ));
            }
            return Err(HtapError::Io(std::io::Error::other(
                "simulated partial write fault (test injection)",
            )));
        }

        if let Err(err) = self.file.write_all(&frame) {
            if let Err(truncate_err) = self.truncate_partial_write_best_effort(write_offset) {
                self.poison(format!(
                    "append_nosync write_all failed at offset {write_offset} ({err}) and the \
                     best-effort truncate back to the last known-good boundary also failed \
                     ({truncate_err}); this journal handle may contain stale bytes past its \
                     recorded valid_end and must not be appended to again until reopened"
                ));
            }
            return Err(HtapError::Io(err));
        }
        self.valid_end = write_offset + frame.len() as u64;
        Ok(write_offset)
    }

    /// Test-only: injects a one-shot simulated partial-write failure into the next
    /// [`Journal::append_nosync`] call, physically writing only the first `partial_len` bytes of
    /// the frame to disk before reporting an I/O error — exactly what a real crashed or short
    /// `write_all` would leave behind — so a test can exercise the truncate-on-failure recovery
    /// path (fix-pass item 6a) deterministically, without needing to defeat the real OS.
    #[doc(hidden)]
    pub fn inject_partial_write_fault_for_test(&mut self, partial_len: usize) {
        self.pending_partial_write_fault_for_test = Some(partial_len);
    }

    /// Test-only: injects a one-shot simulated `fsync` failure into the next internal `fsync`
    /// call, whether it comes from [`Journal::append`]'s own sync-on-write step or from
    /// [`Journal::sync`] — exactly as a real failed `fsync` would after its frame's bytes were
    /// already fully written via `write_all`, so a test can exercise the truncate-and-poison
    /// recovery path (fix-pass round 3, item 1(i)) deterministically, without needing to defeat
    /// the real OS.
    #[doc(hidden)]
    pub fn inject_sync_fault_for_test(&mut self) {
        self.pending_sync_fault_for_test = true;
    }

    /// Test-only: injects a one-shot simulated failure into the next best-effort truncate
    /// performed by [`Journal::truncate_partial_write_best_effort`] (the recovery step run after
    /// a failed append or sync), instead of actually calling `set_len`, so a test can exercise
    /// the poison-on-unrecoverable-truncate-failure path (fix-pass round 3, item 1(ii))
    /// deterministically.
    #[doc(hidden)]
    pub fn inject_truncate_fault_for_test(&mut self) {
        self.pending_truncate_fault_for_test = true;
    }

    /// Returns `true` if an earlier append or sync failure has poisoned this journal handle: see
    /// the [`Self::poisoned`] field's doc. Only a fresh reopen clears it.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned.is_some()
    }

    /// Returns the reason this journal handle was poisoned, if any.
    pub fn poison_reason(&self) -> Option<&str> {
        self.poisoned.as_deref()
    }

    /// Marks this journal handle poisoned with `reason`, keeping the earliest reason if called
    /// more than once.
    fn poison(&mut self, reason: String) {
        if self.poisoned.is_none() {
            tracing::error!(
                path = %self.opts.path.display(),
                %reason,
                "journal poisoned after an unrecoverable write/sync failure; a fresh reopen is required"
            );
            self.poisoned = Some(reason);
        }
    }

    /// Returns an error if this journal handle is poisoned; see the [`Self::poisoned`] field's
    /// doc. Called at the top of every [`Self::append`]/[`Self::append_nosync`]/[`Self::sync`].
    fn check_poisoned(&self) -> Result<()> {
        if let Some(reason) = &self.poisoned {
            return Err(HtapError::Io(std::io::Error::other(format!(
                "journal is poisoned by an earlier unrecoverable write/sync failure and must be \
                 reopened before further appends: {reason}"
            ))));
        }
        Ok(())
    }

    /// Best-effort recovery from a failed write: truncates the file back to `write_offset` (the
    /// last known-good boundary before this failed append) so a later, shorter frame appended
    /// after the caller recovers does not land past leftover garbage bytes, which would otherwise
    /// make [`Journal::open`]/[`Journal::scan`] see unrecoverable middle-of-log corruption
    /// instead of a clean boundary (fix-pass item 6a). Returns an error (rather than swallowing
    /// it) so every call site can poison the journal (fix-pass round 3, item 1(ii)) when this
    /// itself fails, since the file may then contain stale bytes past `valid_end` that a later,
    /// shorter frame appended at `valid_end` would otherwise silently corrupt.
    fn truncate_partial_write_best_effort(&mut self, write_offset: u64) -> Result<()> {
        if self.pending_truncate_fault_for_test {
            self.pending_truncate_fault_for_test = false;
            return Err(HtapError::Io(std::io::Error::other(
                "simulated truncate fault (test injection)",
            )));
        }
        self.file.set_len(write_offset)?;
        self.file.seek(SeekFrom::Start(write_offset))?;
        Ok(())
    }

    /// Fsyncs the file, honoring [`Self::pending_sync_fault_for_test`] if set.
    fn sync_all_checked(&mut self) -> std::io::Result<()> {
        if self.pending_sync_fault_for_test {
            self.pending_sync_fault_for_test = false;
            return Err(std::io::Error::other(
                "simulated fsync fault (test injection)",
            ));
        }
        self.file.sync_all()
    }

    /// Append a [`JournalRecord`] to the journal.
    ///
    /// Synchronously writes the frame to disk. If `sync_on_write` is enabled (the default),
    /// also invokes `fsync`. Returns the byte offset at which the record was written.
    pub fn append(&mut self, record: &JournalRecord) -> Result<u64> {
        self.check_poisoned()?;
        let frame = encode_frame(record, self.opts.max_frame_size)?;
        let write_offset = self.valid_end;

        self.file.seek(SeekFrom::Start(write_offset))?;
        if let Err(err) = self.file.write_all(&frame) {
            if let Err(truncate_err) = self.truncate_partial_write_best_effort(write_offset) {
                self.poison(format!(
                    "append write_all failed at offset {write_offset} ({err}) and the \
                     best-effort truncate back to the last known-good boundary also failed \
                     ({truncate_err}); this journal handle may contain stale bytes past its \
                     recorded valid_end and must not be appended to again until reopened"
                ));
            }
            return Err(HtapError::Io(err));
        }

        if self.opts.sync_on_write {
            if let Err(err) = self.sync_all_checked() {
                // Fix-pass round 3, item 1(i): `write_all` above already succeeded, so the
                // frame's bytes are physically in the file, but not proven durable — a failed
                // fsync means the kernel's page-cache state for them is unknowable. `valid_end`
                // was never advanced past `write_offset`, so best-effort truncate the file back
                // to it (removing the unproven bytes) and poison this handle unconditionally
                // regardless of whether that truncate itself succeeds: a failed fsync alone is
                // reason enough to distrust every future write through this same file handle.
                let truncate_result = self.truncate_partial_write_best_effort(write_offset);
                let mut reason = format!(
                    "fsync failed after appending a frame at offset {write_offset} ({err}); the \
                     kernel's page-cache state for those bytes is now unknowable"
                );
                if let Err(truncate_err) = truncate_result {
                    reason.push_str(&format!(
                        "; the best-effort truncate back to offset {write_offset} also failed: \
                         {truncate_err}"
                    ));
                }
                self.poison(reason);
                return Err(HtapError::Io(err));
            }
        }

        self.valid_end = write_offset + frame.len() as u64;
        Ok(write_offset)
    }

    /// Explicitly fsync the journal file.
    pub fn sync(&mut self) -> Result<()> {
        self.check_poisoned()?;
        if let Err(err) = self.sync_all_checked() {
            // Fix-pass round 3, item 1(i): a failed fsync makes the kernel's page-cache state for
            // whatever was written before this call unknowable, even if a later fsync on the same
            // fd would succeed. Poison unconditionally; only a fresh reopen can be trusted again.
            self.poison(format!(
                "fsync failed ({err}); the kernel's page-cache state for previously appended \
                 bytes is now unknowable"
            ));
            return Err(HtapError::Io(err));
        }
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

    /// Recovers valid records from the journal according to torn-tail and corruption options.
    ///
    /// If torn final records are present and `auto_repair_torn_final` is true, the torn tail is
    /// repaired/truncated and an explanation is returned. If `auto_repair_torn_final` is false,
    /// torn records produce an error. Middle corruption produces an error regardless of options.
    pub fn recover_records(&mut self) -> Result<(Vec<JournalRecord>, Option<String>)> {
        let scan = self.scan()?;
        if let Some((offset, reason)) = scan.middle_corrupt {
            return Err(HtapError::Corruption(format!(
                "journal middle corruption at offset {offset}: {reason}"
            )));
        }

        let mut torn_explanation = None;
        if let Some((offset, reason)) = scan.torn_final {
            if self.opts.auto_repair_torn_final {
                let bytes_truncated = self.repair_torn_final()?;
                torn_explanation = Some(format!(
                    "torn final record at offset {offset} ({bytes_truncated} bytes truncated): {reason}"
                ));
            } else {
                return Err(HtapError::Corruption(format!(
                    "journal torn final record at offset {offset}: {reason}"
                )));
            }
        } else {
            self.valid_end = scan.valid_end;
            self.file.seek(SeekFrom::Start(self.valid_end))?;
        }

        let records = scan.records.into_iter().map(|(_, rec)| rec).collect();
        Ok((records, torn_explanation))
    }

    /// Return the byte length of the valid portion of the journal.
    pub fn valid_bytes(&self) -> u64 {
        self.valid_end
    }

    /// Return the configured options of this journal.
    pub fn options(&self) -> &JournalOptions {
        &self.opts
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
    fn journal_and_wal_frame_encoders_produce_identical_bytes() {
        let payload = b"abc";
        let record = JournalRecord::Intent {
            txn_id: TransactionId::new(1),
            snapshot: Version::new(2),
            participants: vec![ParticipantWork::new(3, payload.to_vec())],
        };

        let serialized_payload = serde_json::to_vec(&record).unwrap();
        let wal_frame = htap_common::envelope::encode_bare_frame(&serialized_payload);
        let journal_frame = encode_frame(&record, DEFAULT_MAX_FRAME_SIZE).unwrap();

        assert_eq!(journal_frame, wal_frame);
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

    fn baseline_record() -> JournalRecord {
        JournalRecord::Commit {
            txn_id: TransactionId::new(7),
            version: Version::new(9),
        }
    }

    #[test]
    fn journal_frame_golden_bytes() {
        let record = baseline_record();
        let frame = encode_frame(&record, DEFAULT_MAX_FRAME_SIZE).unwrap();
        let expected_header: [u8; HEADER_SIZE] = [35, 0, 0, 0, 135, 62, 109, 27];

        assert_eq!(&frame[..HEADER_SIZE], &expected_header);
    }

    #[test]
    fn journal_frame_bad_magic_and_oversized() {
        // Bare journal frames have no magic; zero length is the equivalent invalid-header case.
        let mut bad_header = encode_frame(&baseline_record(), DEFAULT_MAX_FRAME_SIZE).unwrap();
        bad_header[0..4].copy_from_slice(&0u32.to_le_bytes());
        assert!(matches!(
            decode_frame_slice(
                &bad_header,
                0,
                bad_header.len() as u64,
                DEFAULT_MAX_FRAME_SIZE,
            ),
            FrameStatus::TornFinal { .. }
        ));

        let mut oversized = Vec::new();
        oversized.extend_from_slice(&((DEFAULT_MAX_FRAME_SIZE + 1) as u32).to_le_bytes());
        oversized.extend_from_slice(&0u32.to_le_bytes());
        assert!(matches!(
            decode_frame_slice(
                &oversized,
                0,
                oversized.len() as u64,
                DEFAULT_MAX_FRAME_SIZE,
            ),
            FrameStatus::TornFinal { .. }
        ));
    }

    #[test]
    fn journal_frame_bad_version_and_crc() {
        // Bare journal frames have no version; an undecodable CRC-clean payload covers format drift.
        let payload = b"not-a-journal-record";
        let mut bad_format = Vec::new();
        bad_format.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bad_format.extend_from_slice(&crc32c::crc32c(payload).to_le_bytes());
        bad_format.extend_from_slice(payload);
        assert!(matches!(
            decode_frame_slice(
                &bad_format,
                0,
                bad_format.len() as u64,
                DEFAULT_MAX_FRAME_SIZE,
            ),
            FrameStatus::TornFinal { .. }
        ));

        let mut bad_crc = encode_frame(&baseline_record(), DEFAULT_MAX_FRAME_SIZE).unwrap();
        bad_crc[4] ^= 0xff;
        assert!(matches!(
            decode_frame_slice(&bad_crc, 0, bad_crc.len() as u64, DEFAULT_MAX_FRAME_SIZE,),
            FrameStatus::TornFinal { .. }
        ));
    }

    #[test]
    fn journal_frame_size_check_truncated() {
        let mut frame = encode_frame(&baseline_record(), DEFAULT_MAX_FRAME_SIZE).unwrap();
        frame.pop();
        assert!(matches!(
            decode_frame_slice(&frame, 0, frame.len() as u64, DEFAULT_MAX_FRAME_SIZE),
            FrameStatus::TornFinal { .. }
        ));
    }

    #[test]
    fn journal_frame_size_check_trailing() {
        let mut frame = encode_frame(&baseline_record(), DEFAULT_MAX_FRAME_SIZE).unwrap();
        frame.push(0);
        assert!(matches!(
            decode_frame_slice(&frame, 0, frame.len() as u64, DEFAULT_MAX_FRAME_SIZE),
            FrameStatus::Valid { .. }
        ));

        let frame_len = frame.len() - 1;
        let trailing = decode_frame_slice(
            &frame[frame_len..],
            frame_len as u64,
            frame.len() as u64,
            DEFAULT_MAX_FRAME_SIZE,
        );
        assert!(matches!(trailing, FrameStatus::TornFinal { .. }));
    }
}
