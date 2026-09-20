//! Write-ahead log with crash recovery.
//!
//! The WAL is the durability boundary of the engine: a transaction is
//! committed exactly when its [`WalRecord::Commit`] record is durable on disk.
//! It is **shared by both storage formats** (see ADR-004), so records carry a
//! `partition_id` and are format-agnostic; one transaction may touch a
//! row-format and a column-format partition under a single commit record.
//!
//! # On-disk format
//!
//! The log is a *directory* of segment files named `{first_lsn:020}.wal`, so
//! the files sort lexicographically in LSN order. A new segment is started
//! once the active one reaches [`WalOptions::max_segment_bytes`].
//!
//! Every record is framed as:
//!
//! ```text
//! | payload_len: u32 LE | crc32c: u32 LE | payload: [u8; payload_len] |
//! ```
//!
//! The CRC is CRC32-C (Castagnoli) over the **payload bytes only**. Putting
//! the length and the CRC *before* the payload lets the reader validate the
//! frame header — and bound the allocation — before it allocates anything.
//!
//! # Torn writes
//!
//! A crash in the middle of `write(2)` leaves a partial record at the end of
//! the active segment. This is expected, not exceptional. Replay stops
//! cleanly at the first record that is
//!
//! - truncated (fewer bytes remain in the file than `payload_len` demands),
//! - CRC-mismatched, or
//! - implausibly long (`payload_len > `[`MAX_PAYLOAD_BYTES`]),
//!
//! returns every record before it, and reports where it stopped in
//! [`WalReplay::truncated_at`]. It is **not** an error.
//!
//! Corruption in the *middle* of a segment (a valid record following a bad
//! one) is indistinguishable from a torn tail with this framing: there is no
//! resynchronisation marker to scan forward to, and guessing would risk
//! replaying garbage as data. By design we therefore stop at the first bad
//! record and **treat the entire remainder of the log as lost**, including any
//! subsequent segments.
//!
//! # Atomicity
//!
//! Replay alone is not enough: a crash can land between a transaction's data
//! records and its commit marker. [`WalReplay::committed_records`] filters the
//! replayed stream down to records of transactions that actually committed,
//! which is what makes recovery lose nothing committed and expose nothing
//! uncommitted.
//!
//! # Segment Continuity
//!
//! Multi-segment logs must be contiguous. The first retained segment may start
//! at any LSN (to permit garbage collection of older segments), but every
//! subsequent segment file name's LSN must strictly equal the `next_lsn` from the
//! preceding segment's scan. Any gap or overlap between segments is rejected
//! as log corruption.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};

use htap_common::{
    bytecursor::ByteReader, envelope::encode_bare_frame, HtapError, Result, Row, Version,
};
use serde::{Deserialize, Serialize};

/// Size of the fixed record header: `payload_len: u32` + `crc32c: u32`.
const HEADER_BYTES: u64 = 8;

/// Upper bound on a single record's payload, 64 MiB.
///
/// A frame claiming more than this is treated as corruption rather than
/// trusted, which is what stops a garbled length field from turning into a
/// multi-gigabyte allocation during recovery.
pub const MAX_PAYLOAD_BYTES: u64 = 64 * 1024 * 1024;

/// Default segment roll-over size, 64 MiB.
pub const DEFAULT_MAX_SEGMENT_BYTES: u64 = 64 * 1024 * 1024;

/// Extension of a WAL segment file.
const SEGMENT_EXT: &str = "wal";

/// Number of zero-padded digits in a segment file name.
const SEGMENT_NAME_DIGITS: usize = 20;

/// Monotonically increasing log sequence number.
///
/// An LSN identifies one record in the log. It is a record counter, not a byte
/// offset: the first record of segment `{n:020}.wal` has LSN `n`, and each
/// following record in that segment is one greater.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
pub struct Lsn(u64);

impl Lsn {
    /// LSN of the very first record ever written to a log.
    pub const ZERO: Self = Self(0);

    /// Create an LSN with the given raw value.
    #[inline]
    pub const fn new(val: u64) -> Self {
        Self(val)
    }

    /// Return the raw `u64`.
    #[inline]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Return the strictly next LSN.
    #[inline]
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl std::fmt::Display for Lsn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "lsn#{}", self.0)
    }
}

/// A single logical entry in the write-ahead log.
///
/// Payloads are serialised with `serde_json`. That is deliberate for now: it
/// is simple, self-describing and a corrupt record can be eyeballed with
/// `xxd`. A compact binary encoding (bincode or a hand-rolled format) is a
/// later optimisation — it changes only [`encode_frame`] and the decode step
/// in [`read_segment`], not the framing or the recovery logic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WalRecord {
    /// A row written (insert or update) into a partition at a version.
    Put {
        /// Owning transaction.
        txn_id: u64,
        /// Partition the row belongs to; may be row- or column-format.
        partition_id: u64,
        /// Encoded primary key (see `htap_common::keycodec`).
        key: Vec<u8>,
        /// The row image.
        row: Row,
        /// MVCC version the write becomes visible at.
        version: Version,
    },
    /// A row deleted from a partition at a version.
    Delete {
        /// Owning transaction.
        txn_id: u64,
        /// Partition the row belongs to.
        partition_id: u64,
        /// Encoded primary key of the deleted row.
        key: Vec<u8>,
        /// MVCC version the deletion becomes visible at.
        version: Version,
    },
    /// Transaction commit marker. Only records belonging to a committed txn
    /// are replayed.
    Commit {
        /// The transaction that committed.
        txn_id: u64,
        /// Commit version.
        version: Version,
    },
    /// Transaction abort marker.
    Abort {
        /// The transaction that aborted.
        txn_id: u64,
    },
    /// Checkpoint: all data at or below this version is durable in SSTs, so
    /// WAL segments consisting entirely of records at or below it can be GCed.
    Checkpoint {
        /// Highest version known durable outside the WAL.
        version: Version,
    },
}

impl WalRecord {
    /// The MVCC version this record pins, if any.
    ///
    /// [`WalRecord::Abort`] pins nothing: an aborted transaction's records are
    /// dropped on replay regardless, so it never holds a segment back from GC.
    pub fn version(&self) -> Option<Version> {
        match self {
            WalRecord::Put { version, .. }
            | WalRecord::Delete { version, .. }
            | WalRecord::Commit { version, .. }
            | WalRecord::Checkpoint { version } => Some(*version),
            WalRecord::Abort { .. } => None,
        }
    }

    /// The transaction this record belongs to, if any.
    pub fn txn_id(&self) -> Option<u64> {
        match self {
            WalRecord::Put { txn_id, .. }
            | WalRecord::Delete { txn_id, .. }
            | WalRecord::Commit { txn_id, .. }
            | WalRecord::Abort { txn_id } => Some(*txn_id),
            WalRecord::Checkpoint { .. } => None,
        }
    }

    /// Whether this is a transaction data record (`Put`/`Delete`) rather than
    /// a marker.
    pub fn is_data(&self) -> bool {
        matches!(self, WalRecord::Put { .. } | WalRecord::Delete { .. })
    }
}

/// Configuration for opening a [`Wal`].
///
/// There is deliberately no `Default`: `dir` has no sensible default and
/// silently defaulting it would be a way to write a log into the wrong place.
/// Use [`WalOptions::new`] and the `with_*` builders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalOptions {
    /// Directory holding the segment files. Created if missing.
    pub dir: PathBuf,
    /// Roll to a new segment once the active one reaches this size.
    pub max_segment_bytes: u64,
    /// Whether [`Wal::append_commit`] fsyncs. Turning this off trades
    /// durability for throughput and is only safe in tests or for data that
    /// may be lost on power failure.
    pub sync_on_commit: bool,
}

impl WalOptions {
    /// Options for a WAL in `dir` with the default segment size and
    /// `sync_on_commit` enabled.
    pub fn new(dir: impl Into<PathBuf>) -> WalOptions {
        WalOptions {
            dir: dir.into(),
            max_segment_bytes: DEFAULT_MAX_SEGMENT_BYTES,
            sync_on_commit: true,
        }
    }

    /// Override the segment roll-over size.
    #[must_use]
    pub fn with_max_segment_bytes(mut self, bytes: u64) -> WalOptions {
        self.max_segment_bytes = bytes;
        self
    }

    /// Override whether commits fsync.
    #[must_use]
    pub fn with_sync_on_commit(mut self, sync: bool) -> WalOptions {
        self.sync_on_commit = sync;
        self
    }
}

/// Bookkeeping for one segment file held by an open [`Wal`].
#[derive(Debug, Clone)]
struct SegmentMeta {
    /// LSN of the segment's first record; also its file name.
    first_lsn: Lsn,
    /// Path of the segment file.
    path: PathBuf,
    /// Highest version of any record in the segment, `None` if it holds no
    /// version-bearing record.
    max_version: Option<Version>,
}

/// An open write-ahead log.
///
/// Appends go to the active segment. Records are written with a single
/// `write_all` of a fully built frame and are **not** buffered in user space:
/// buffering would mean `sync()` could return without the bytes having reached
/// the kernel at all.
#[derive(Debug)]
pub struct Wal {
    opts: WalOptions,
    /// All live segments in LSN order. The last one is the active segment
    /// whenever `active` is `Some`.
    segments: Vec<SegmentMeta>,
    /// Handle to the active segment, `None` until the first append.
    active: Option<File>,
    /// Byte length of the active segment.
    active_len: u64,
    /// LSN the next appended record will receive.
    next_lsn: Lsn,
}

impl Wal {
    /// Open or create a WAL in `opts.dir`, recovering existing segments.
    ///
    /// Recovery scans the segments to re-establish the next LSN and, if the
    /// last segment ends in a torn or corrupt record, **truncates it back to
    /// the last valid record boundary**. Without that truncation a subsequent
    /// append would land behind unreadable bytes and be silently invisible to
    /// every future replay.
    pub fn open(opts: WalOptions) -> Result<Wal> {
        std::fs::create_dir_all(&opts.dir)?;
        // The directory entry for a freshly created WAL directory is itself
        // only durable after its parent is fsynced; we fsync the WAL directory
        // on segment creation below, which covers the common case.
        let listed = list_segments(&opts.dir)?;

        let mut segments = Vec::with_capacity(listed.len());
        let mut next_lsn = Lsn::ZERO;
        let mut active = None;
        let mut active_len = 0u64;

        let last_idx = listed.len().saturating_sub(1);
        let mut expected_next_lsn: Option<Lsn> = None;
        for (idx, (first_lsn, path)) in listed.into_iter().enumerate() {
            if let Some(expected) = expected_next_lsn {
                if first_lsn != expected {
                    return Err(HtapError::Corruption(format!(
                        "WAL segment continuity error: segment {} starts at {first_lsn}, expected {expected}",
                        path.display()
                    )));
                }
            }
            let scan = read_segment(&path, first_lsn)?;
            if let Some(at) = scan.stopped {
                if idx == last_idx {
                    tracing::warn!(
                        segment = %path.display(),
                        %at,
                        "torn tail in active WAL segment; truncating to last valid record"
                    );
                } else {
                    tracing::warn!(
                        segment = %path.display(),
                        %at,
                        "corrupt record in a non-final WAL segment; \
                         everything from here on is unrecoverable by design"
                    );
                }
            }
            segments.push(SegmentMeta {
                first_lsn,
                path: path.clone(),
                max_version: scan.max_version,
            });

            if idx == last_idx {
                // O_APPEND, not plain O_WRONLY: a write(2) on a plain handle
                // starts at offset 0 and would overwrite the segment's first
                // records instead of extending it.
                let file = OpenOptions::new().read(true).append(true).open(&path)?;
                // Drop the torn tail so appends continue from a clean boundary.
                if scan.valid_end != scan.file_len {
                    file.set_len(scan.valid_end)?;
                    file.sync_all()?;
                }
                active_len = scan.valid_end;
                next_lsn = scan.next_lsn;
                active = Some(file);
            }
            expected_next_lsn = Some(scan.next_lsn);
        }

        Ok(Wal {
            opts,
            segments,
            active,
            active_len,
            next_lsn,
        })
    }

    /// Append a record, returning its LSN. Does **not** fsync.
    pub fn append(&mut self, rec: &WalRecord) -> Result<Lsn> {
        let frame = encode_frame(rec)?;

        // Roll before writing, never after, so a segment always contains at
        // least one record and its name always equals its first record's LSN.
        if self.active.is_none()
            || (self.active_len > 0 && self.active_len >= self.opts.max_segment_bytes)
        {
            self.roll_segment()?;
        }

        let lsn = self.next_lsn;
        let file = self
            .active
            .as_mut()
            .ok_or_else(|| HtapError::Internal("WAL has no active segment".into()))?;
        // Write the frame straight through to the file handle. Any userspace
        // buffering here would be a durability bug: sync() fsyncs the file
        // descriptor, so bytes still sitting in a Vec would not be made
        // durable and append_commit() would report success for data that is
        // not on disk.
        file.write_all(&frame)?;

        self.active_len += frame.len() as u64;
        self.next_lsn = lsn.next();
        if let (Some(meta), Some(v)) = (self.segments.last_mut(), rec.version()) {
            meta.max_version = Some(meta.max_version.map_or(v, |cur| cur.max(v)));
        }
        Ok(lsn)
    }

    /// fsync the active segment.
    pub fn sync(&mut self) -> Result<()> {
        if let Some(file) = self.active.as_ref() {
            file.sync_all()?;
        }
        Ok(())
    }

    /// Append and, if [`WalOptions::sync_on_commit`], fsync. Use this for
    /// [`WalRecord::Commit`] records: when it returns, the transaction is
    /// durable.
    pub fn append_commit(&mut self, rec: &WalRecord) -> Result<Lsn> {
        let lsn = self.append(rec)?;
        if self.opts.sync_on_commit {
            self.sync()?;
        }
        Ok(lsn)
    }

    /// Replay every record in `dir`, in LSN order.
    ///
    /// A torn or corrupt tail is not an error; see the module docs.
    pub fn replay(dir: &Path) -> Result<WalReplay> {
        let mut replay = WalReplay {
            records: Vec::new(),
            truncated_at: None,
            segments_read: 0,
        };
        if !dir.exists() {
            return Ok(replay);
        }

        let mut expected_next_lsn: Option<Lsn> = None;
        for (first_lsn, path) in list_segments(dir)? {
            if let Some(expected) = expected_next_lsn {
                if first_lsn != expected {
                    return Err(HtapError::Corruption(format!(
                        "WAL segment continuity error: segment {} starts at {first_lsn}, expected {expected}",
                        path.display()
                    )));
                }
            }
            replay.segments_read += 1;
            let scan = read_segment(&path, first_lsn)?;
            replay.records.extend(scan.records);
            if let Some(at) = scan.stopped {
                // Stop at the first bad record; the rest of the log — this
                // segment's remainder and every later segment — is lost.
                replay.truncated_at = Some(at);
                break;
            }
            expected_next_lsn = Some(scan.next_lsn);
        }
        Ok(replay)
    }

    /// Delete segments fully superseded by a checkpoint at `version`.
    ///
    /// A segment is removed only if every version-bearing record in it is at
    /// or below `version` *and* every earlier segment is removable too: GC
    /// works on a prefix, so it can never punch a hole in the log. The active
    /// segment is never removed.
    ///
    /// Returns the number of segment files deleted.
    pub fn gc(&mut self, up_to: Version) -> Result<usize> {
        let keep_from = self.segments.len().saturating_sub(1);
        let mut removed = 0usize;
        while removed < keep_from {
            let meta = &self.segments[removed];
            let superseded = meta.max_version.is_none_or(|v| v <= up_to);
            if !superseded {
                break;
            }
            match std::fs::remove_file(&meta.path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            tracing::debug!(segment = %meta.path.display(), %up_to, "GCed WAL segment");
            removed += 1;
        }
        if removed > 0 {
            self.segments.drain(..removed);
            fsync_dir(&self.opts.dir)?;
        }
        Ok(removed)
    }

    /// The LSN the next appended record will receive.
    pub fn current_lsn(&self) -> Lsn {
        self.next_lsn
    }

    /// Paths of the live segment files, in LSN order.
    pub fn segment_paths(&self) -> Vec<PathBuf> {
        self.segments.iter().map(|s| s.path.clone()).collect()
    }

    /// First LSN of each live segment, in order. After [`Wal::gc`] the first
    /// entry is the oldest LSN still recoverable from this log.
    pub fn segment_first_lsns(&self) -> Vec<Lsn> {
        self.segments.iter().map(|s| s.first_lsn).collect()
    }

    /// LSN of the oldest record still present in the log, or `None` if the log
    /// holds no segments yet.
    pub fn oldest_lsn(&self) -> Option<Lsn> {
        self.segments.first().map(|s| s.first_lsn)
    }

    /// The options this WAL was opened with.
    pub fn options(&self) -> &WalOptions {
        &self.opts
    }

    /// Start a new active segment named after the next LSN.
    fn roll_segment(&mut self) -> Result<()> {
        // Flush the outgoing segment so a crash right after the roll cannot
        // lose records that the new segment implicitly claims are behind it.
        if let Some(file) = self.active.as_ref() {
            file.sync_all()?;
        }

        let path = self.opts.dir.join(segment_file_name(self.next_lsn));
        let file = OpenOptions::new()
            .append(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| {
                HtapError::Io(std::io::Error::new(
                    e.kind(),
                    format!("creating WAL segment {}: {e}", path.display()),
                ))
            })?;

        // Creating a file makes the *file* durable once synced, but not its
        // name: the directory entry lives in the parent directory's own
        // metadata. Without this fsync a crash can leave a WAL whose newest
        // segment simply does not exist any more, silently losing every commit
        // in it. This is the classic missed-fsync durability bug.
        fsync_dir(&self.opts.dir)?;

        self.segments.push(SegmentMeta {
            first_lsn: self.next_lsn,
            path,
            max_version: None,
        });
        self.active = Some(file);
        self.active_len = 0;
        Ok(())
    }
}

/// Outcome of replaying a WAL directory.
#[derive(Debug, Clone, PartialEq)]
pub struct WalReplay {
    /// Every record read, paired with its LSN, in LSN order.
    pub records: Vec<(Lsn, WalRecord)>,
    /// Set when replay stopped early at a torn or corrupt record. Holds the
    /// LSN that record *would* have had.
    pub truncated_at: Option<Lsn>,
    /// Number of segment files opened.
    pub segments_read: usize,
}

impl WalReplay {
    /// Records belonging to transactions that have a [`WalRecord::Commit`]
    /// record, in LSN order, with the `Commit`/`Abort` markers removed.
    ///
    /// Records of transactions with no commit marker — because the process
    /// died mid-transaction, or because they explicitly aborted — are dropped.
    /// An `Abort` wins over a `Commit` for the same txn id, which can only
    /// happen in a malformed log. [`WalRecord::Checkpoint`] belongs to no
    /// transaction and is not returned; read it from [`Self::records`].
    ///
    /// This is what makes a crash mid-transaction lose nothing committed and
    /// expose nothing uncommitted.
    pub fn committed_records(&self) -> Vec<(Lsn, WalRecord)> {
        let mut committed: HashSet<u64> = HashSet::new();
        let mut aborted: HashSet<u64> = HashSet::new();
        for (_, rec) in &self.records {
            match rec {
                WalRecord::Commit { txn_id, .. } => {
                    committed.insert(*txn_id);
                }
                WalRecord::Abort { txn_id } => {
                    aborted.insert(*txn_id);
                }
                _ => {}
            }
        }

        self.records
            .iter()
            .filter(|(_, rec)| match rec {
                WalRecord::Put { txn_id, .. } | WalRecord::Delete { txn_id, .. } => {
                    committed.contains(txn_id) && !aborted.contains(txn_id)
                }
                _ => false,
            })
            .cloned()
            .collect()
    }

    /// Ids of transactions with a commit marker and no abort marker.
    pub fn committed_txn_ids(&self) -> HashSet<u64> {
        let mut committed: HashSet<u64> = HashSet::new();
        let mut aborted: HashSet<u64> = HashSet::new();
        for (_, rec) in &self.records {
            match rec {
                WalRecord::Commit { txn_id, .. } => {
                    committed.insert(*txn_id);
                }
                WalRecord::Abort { txn_id } => {
                    aborted.insert(*txn_id);
                }
                _ => {}
            }
        }
        committed.retain(|id| !aborted.contains(id));
        committed
    }

    /// Highest checkpoint version in the replayed log, if any.
    pub fn last_checkpoint(&self) -> Option<Version> {
        self.records
            .iter()
            .filter_map(|(_, rec)| match rec {
                WalRecord::Checkpoint { version } => Some(*version),
                _ => None,
            })
            .max()
    }
}

/// Serialise a record into its on-disk frame.
fn encode_frame(rec: &WalRecord) -> Result<Vec<u8>> {
    let payload = serde_json::to_vec(rec)
        .map_err(|e| HtapError::Internal(format!("serialising WAL record: {e}")))?;
    if payload.len() as u64 > MAX_PAYLOAD_BYTES {
        return Err(HtapError::InvalidArgument(format!(
            "WAL record of {} bytes exceeds the {MAX_PAYLOAD_BYTES} byte limit",
            payload.len()
        )));
    }
    Ok(encode_bare_frame(&payload))
}

/// Result of scanning one segment file.
struct SegmentScan {
    /// Valid records with their LSNs.
    records: Vec<(Lsn, WalRecord)>,
    /// LSN following the last valid record.
    next_lsn: Lsn,
    /// Byte offset just past the last valid record.
    valid_end: u64,
    /// Total size of the file.
    file_len: u64,
    /// LSN of the first torn/corrupt record, if the scan stopped early.
    stopped: Option<Lsn>,
    /// Highest version among the valid records.
    max_version: Option<Version>,
}

/// Read every intact record from one segment, stopping at the first bad frame.
fn read_segment(path: &Path, first_lsn: Lsn) -> Result<SegmentScan> {
    let file = File::open(path)?;
    let file_len = file.metadata()?.len();
    let mut reader = BufReader::new(file);

    let mut scan = SegmentScan {
        records: Vec::new(),
        next_lsn: first_lsn,
        valid_end: 0,
        file_len,
        stopped: None,
        max_version: None,
    };

    let mut pos: u64 = 0;
    loop {
        let remaining = file_len - pos;
        if remaining == 0 {
            break; // Clean end of segment.
        }
        if remaining < HEADER_BYTES {
            // Torn: the crash landed inside the frame header itself.
            scan.stopped = Some(scan.next_lsn);
            break;
        }

        let mut header = [0u8; HEADER_BYTES as usize];
        reader.read_exact(&mut header)?;
        pos += HEADER_BYTES;
        let mut header_reader = ByteReader::new(&header);
        let payload_len = header_reader
            .read_u32_le()
            .expect("fixed-size WAL frame header") as u64;
        let expected_crc = header_reader
            .read_u32_le()
            .expect("fixed-size WAL frame header");

        // Validate the length *before* allocating. A zero length is never
        // produced by the writer, and anything above the cap is a garbled
        // field rather than a real record — trusting it would be an OOM.
        if payload_len == 0 || payload_len > MAX_PAYLOAD_BYTES {
            scan.stopped = Some(scan.next_lsn);
            break;
        }
        if file_len - pos < payload_len {
            // Torn: fewer bytes remain than the frame claims.
            scan.stopped = Some(scan.next_lsn);
            break;
        }

        let mut payload = vec![0u8; payload_len as usize];
        reader.read_exact(&mut payload)?;
        pos += payload_len;

        if crc32c::crc32c(&payload) != expected_crc {
            scan.stopped = Some(scan.next_lsn);
            break;
        }

        let rec: WalRecord = match serde_json::from_slice(&payload) {
            Ok(rec) => rec,
            Err(_) => {
                // CRC-clean but undecodable: a format change or a collision.
                // Treat it exactly like corruption.
                scan.stopped = Some(scan.next_lsn);
                break;
            }
        };

        if let Some(v) = rec.version() {
            scan.max_version = Some(scan.max_version.map_or(v, |cur: Version| cur.max(v)));
        }
        scan.records.push((scan.next_lsn, rec));
        scan.next_lsn = scan.next_lsn.next();
        scan.valid_end = pos;
    }

    Ok(scan)
}

/// File name of the segment whose first record has LSN `lsn`.
fn segment_file_name(lsn: Lsn) -> String {
    format!(
        "{:0width$}.{SEGMENT_EXT}",
        lsn.get(),
        width = SEGMENT_NAME_DIGITS
    )
}

/// Parse a segment file name back into its first LSN.
fn parse_segment_name(name: &str) -> Option<Lsn> {
    let stem = name.strip_suffix(&format!(".{SEGMENT_EXT}"))?;
    if stem.len() != SEGMENT_NAME_DIGITS || !stem.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    stem.parse::<u64>().ok().map(Lsn::new)
}

/// List the segment files in `dir`, sorted by first LSN. Files that do not
/// match the naming scheme are ignored.
fn list_segments(dir: &Path) -> Result<Vec<(Lsn, PathBuf)>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(lsn) = parse_segment_name(name) {
            out.push((lsn, entry.path()));
        }
    }
    out.sort_by_key(|(lsn, _)| *lsn);
    Ok(out)
}

/// fsync a directory so that entries created or removed in it are durable.
// Deliberately not moved to the shared helper: unlike the five migrated
// callers, this function is not Unix-gated, so migrating it in either
// direction would change untested non-Unix behaviour.
fn fsync_dir(dir: &Path) -> Result<()> {
    let handle = File::open(dir)?;
    handle.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use htap_common::Value;
    use std::io::Seek;

    fn row(n: i64) -> Row {
        Row::new(vec![Value::Int64(n), Value::String(format!("row-{n}"))])
    }

    fn put(txn: u64, n: i64) -> WalRecord {
        WalRecord::Put {
            txn_id: txn,
            partition_id: 7,
            key: n.to_be_bytes().to_vec(),
            row: row(n),
            version: Version::new(n as u64 + 1),
        }
    }

    fn commit(txn: u64, v: u64) -> WalRecord {
        WalRecord::Commit {
            txn_id: txn,
            version: Version::new(v),
        }
    }

    #[test]
    fn lsn_basics() {
        assert_eq!(Lsn::ZERO.get(), 0);
        assert_eq!(Lsn::new(4).next(), Lsn::new(5));
        assert!(Lsn::new(1) < Lsn::new(2));
        assert_eq!(Lsn::new(9).to_string(), "lsn#9");
        let json = serde_json::to_string(&Lsn::new(12)).unwrap();
        assert_eq!(serde_json::from_str::<Lsn>(&json).unwrap(), Lsn::new(12));
    }

    #[test]
    fn segment_names_round_trip_and_sort() {
        let name = segment_file_name(Lsn::new(42));
        assert_eq!(name, "00000000000000000042.wal");
        assert_eq!(parse_segment_name(&name), Some(Lsn::new(42)));
        assert_eq!(parse_segment_name("nope.wal"), None);
        assert_eq!(parse_segment_name("00000000000000000042.log"), None);
        assert_eq!(parse_segment_name("0000000000000000004x.wal"), None);
        // Zero padding is what makes lexicographic order equal LSN order.
        assert!(segment_file_name(Lsn::new(9)) < segment_file_name(Lsn::new(10)));
    }

    #[test]
    fn frame_layout_is_len_crc_payload() {
        let rec = commit(1, 2);
        let frame = encode_frame(&rec).unwrap();
        let len = u32::from_le_bytes(frame[0..4].try_into().unwrap()) as usize;
        let crc = u32::from_le_bytes(frame[4..8].try_into().unwrap());
        assert_eq!(frame.len(), 8 + len);
        assert_eq!(crc, crc32c::crc32c(&frame[8..]));
        assert_eq!(
            serde_json::from_slice::<WalRecord>(&frame[8..]).unwrap(),
            rec
        );
    }

    #[test]
    fn round_trip_mixed_records() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = Wal::open(WalOptions::new(dir.path())).unwrap();
        assert_eq!(wal.current_lsn(), Lsn::ZERO);

        let recs = vec![
            put(1, 1),
            WalRecord::Delete {
                txn_id: 1,
                partition_id: 3,
                key: vec![0, 1, 2],
                version: Version::new(3),
            },
            commit(1, 4),
            WalRecord::Abort { txn_id: 2 },
            WalRecord::Checkpoint {
                version: Version::new(4),
            },
        ];
        for (i, rec) in recs.iter().enumerate() {
            assert_eq!(wal.append(rec).unwrap(), Lsn::new(i as u64));
        }
        wal.sync().unwrap();
        assert_eq!(wal.current_lsn(), Lsn::new(5));

        let replay = Wal::replay(dir.path()).unwrap();
        assert_eq!(replay.truncated_at, None);
        assert_eq!(replay.segments_read, 1);
        let got: Vec<WalRecord> = replay.records.iter().map(|(_, r)| r.clone()).collect();
        assert_eq!(got, recs);
        let lsns: Vec<Lsn> = replay.records.iter().map(|(l, _)| *l).collect();
        assert_eq!(lsns, (0..5).map(Lsn::new).collect::<Vec<_>>());
    }

    #[test]
    fn reopen_continues_lsn_sequence() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut wal = Wal::open(WalOptions::new(dir.path())).unwrap();
            wal.append(&put(1, 1)).unwrap();
            wal.append_commit(&commit(1, 2)).unwrap();
        }
        let mut wal = Wal::open(WalOptions::new(dir.path())).unwrap();
        assert_eq!(wal.current_lsn(), Lsn::new(2));
        assert_eq!(wal.append(&put(2, 5)).unwrap(), Lsn::new(2));
        drop(wal);

        let replay = Wal::replay(dir.path()).unwrap();
        assert_eq!(replay.records.len(), 3);
    }

    #[test]
    fn empty_and_missing_directories_open_cleanly() {
        // Missing directory: created on open.
        let base = tempfile::tempdir().unwrap();
        let missing = base.path().join("does/not/exist");
        assert!(!missing.exists());
        let wal = Wal::open(WalOptions::new(&missing)).unwrap();
        assert_eq!(wal.current_lsn(), Lsn::ZERO);
        assert!(wal.segment_paths().is_empty());
        assert_eq!(wal.oldest_lsn(), None);
        assert!(missing.exists());

        // Replay of a missing directory is empty, not an error.
        let never = base.path().join("never");
        let replay = Wal::replay(&never).unwrap();
        assert_eq!(replay.records.len(), 0);
        assert_eq!(replay.segments_read, 0);
        assert_eq!(replay.truncated_at, None);

        // Existing but empty directory.
        let empty = tempfile::tempdir().unwrap();
        let wal = Wal::open(WalOptions::new(empty.path())).unwrap();
        assert_eq!(wal.current_lsn(), Lsn::ZERO);
        let replay = Wal::replay(empty.path()).unwrap();
        assert!(replay.records.is_empty());
        assert_eq!(replay.segments_read, 0);
    }

    #[test]
    fn unrelated_files_in_the_directory_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = Wal::open(WalOptions::new(dir.path())).unwrap();
        wal.append(&commit(1, 2)).unwrap();
        wal.sync().unwrap();
        std::fs::write(dir.path().join("README.txt"), b"not a segment").unwrap();
        std::fs::write(dir.path().join("0001.wal"), b"wrong width").unwrap();

        let replay = Wal::replay(dir.path()).unwrap();
        assert_eq!(replay.segments_read, 1);
        assert_eq!(replay.records.len(), 1);
        assert_eq!(replay.truncated_at, None);
    }

    #[test]
    fn torn_tail_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = Wal::open(WalOptions::new(dir.path())).unwrap();
        for i in 0..8 {
            wal.append(&put(1, i)).unwrap();
        }
        wal.sync().unwrap();
        let path = wal.segment_paths()[0].clone();
        drop(wal);
        let full_len = std::fs::metadata(&path).unwrap().len();

        // Cut into the middle of the final record.
        let f = OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(full_len - 5).unwrap();
        f.sync_all().unwrap();

        let replay = Wal::replay(dir.path()).unwrap();
        assert_eq!(replay.records.len(), 7);
        assert_eq!(replay.truncated_at, Some(Lsn::new(7)));
    }

    #[test]
    fn bad_crc_stops_replay_there() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = Wal::open(WalOptions::new(dir.path())).unwrap();
        for i in 0..5 {
            wal.append(&put(1, i)).unwrap();
        }
        wal.sync().unwrap();
        let path = wal.segment_paths()[0].clone();
        // Length of the first frame, so we can flip a byte in the second one.
        let first = encode_frame(&put(1, 0)).unwrap().len() as u64;
        drop(wal);

        let mut f = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        // Byte inside the second record's payload.
        f.seek(std::io::SeekFrom::Start(first + HEADER_BYTES + 2))
            .unwrap();
        let mut b = [0u8; 1];
        {
            let mut r = File::open(&path).unwrap();
            r.seek(std::io::SeekFrom::Start(first + HEADER_BYTES + 2))
                .unwrap();
            r.read_exact(&mut b).unwrap();
        }
        f.write_all(&[b[0] ^ 0xff]).unwrap();
        f.sync_all().unwrap();

        let replay = Wal::replay(dir.path()).unwrap();
        assert_eq!(replay.records.len(), 1);
        assert_eq!(replay.truncated_at, Some(Lsn::new(1)));
    }

    #[test]
    fn implausible_length_is_corruption_not_an_oom() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = Wal::open(WalOptions::new(dir.path())).unwrap();
        wal.append(&put(1, 0)).unwrap();
        wal.sync().unwrap();
        let path = wal.segment_paths()[0].clone();
        let first = encode_frame(&put(1, 0)).unwrap().len() as u64;
        drop(wal);

        // Append a frame claiming ~4 GiB of payload. If the reader trusted it,
        // this would try to allocate 4 GiB.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(&u32::MAX.to_le_bytes()).unwrap();
        f.write_all(&0u32.to_le_bytes()).unwrap();
        f.write_all(b"tiny").unwrap();
        f.sync_all().unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), first + 12);

        let replay = Wal::replay(dir.path()).unwrap();
        assert_eq!(replay.records.len(), 1);
        assert_eq!(replay.truncated_at, Some(Lsn::new(1)));
    }

    #[test]
    fn zero_length_frame_is_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = Wal::open(WalOptions::new(dir.path())).unwrap();
        wal.append(&put(1, 0)).unwrap();
        wal.sync().unwrap();
        let path = wal.segment_paths()[0].clone();
        drop(wal);

        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(&0u32.to_le_bytes()).unwrap();
        f.write_all(&0u32.to_le_bytes()).unwrap();
        f.sync_all().unwrap();

        let replay = Wal::replay(dir.path()).unwrap();
        assert_eq!(replay.records.len(), 1);
        assert_eq!(replay.truncated_at, Some(Lsn::new(1)));
    }

    #[test]
    fn open_truncates_a_torn_tail_so_appends_stay_visible() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = Wal::open(WalOptions::new(dir.path())).unwrap();
        for i in 0..4 {
            wal.append(&put(1, i)).unwrap();
        }
        wal.sync().unwrap();
        let path = wal.segment_paths()[0].clone();
        drop(wal);

        let len = std::fs::metadata(&path).unwrap().len();
        let f = OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(len - 3).unwrap();
        f.sync_all().unwrap();

        let mut wal = Wal::open(WalOptions::new(dir.path())).unwrap();
        assert_eq!(wal.current_lsn(), Lsn::new(3));
        wal.append(&commit(1, 9)).unwrap();
        wal.sync().unwrap();
        drop(wal);

        let replay = Wal::replay(dir.path()).unwrap();
        assert_eq!(replay.truncated_at, None, "tail should have been repaired");
        assert_eq!(replay.records.len(), 4);
        assert_eq!(replay.records[3].1, commit(1, 9));
    }

    #[test]
    fn committed_records_keeps_only_committed_transactions() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = Wal::open(WalOptions::new(dir.path())).unwrap();
        wal.append(&put(1, 1)).unwrap();
        wal.append(&put(2, 2)).unwrap(); // txn 2 never commits
        wal.append(&put(1, 3)).unwrap();
        wal.append_commit(&commit(1, 10)).unwrap();
        wal.append(&put(2, 4)).unwrap();
        drop(wal);

        let replay = Wal::replay(dir.path()).unwrap();
        assert_eq!(replay.records.len(), 5);
        let committed = replay.committed_records();
        assert_eq!(committed.len(), 2);
        assert!(committed.iter().all(|(_, r)| r.txn_id() == Some(1)));
        assert!(committed.iter().all(|(_, r)| r.is_data()));
        assert_eq!(committed[0].0, Lsn::new(0));
        assert_eq!(committed[1].0, Lsn::new(2));
        assert_eq!(replay.committed_txn_ids(), HashSet::from([1]));
    }

    #[test]
    fn explicit_abort_drops_its_records() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = Wal::open(WalOptions::new(dir.path())).unwrap();
        wal.append(&put(1, 1)).unwrap();
        wal.append(&put(2, 2)).unwrap();
        wal.append(&WalRecord::Abort { txn_id: 2 }).unwrap();
        wal.append_commit(&commit(1, 10)).unwrap();
        drop(wal);

        let replay = Wal::replay(dir.path()).unwrap();
        let committed = replay.committed_records();
        assert_eq!(committed.len(), 1);
        assert_eq!(committed[0].1.txn_id(), Some(1));
        assert_eq!(replay.committed_txn_ids(), HashSet::from([1]));
    }

    #[test]
    fn abort_beats_commit_for_the_same_txn() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = Wal::open(WalOptions::new(dir.path())).unwrap();
        wal.append(&put(3, 1)).unwrap();
        wal.append(&commit(3, 5)).unwrap();
        wal.append(&WalRecord::Abort { txn_id: 3 }).unwrap();
        drop(wal);

        let replay = Wal::replay(dir.path()).unwrap();
        assert!(replay.committed_records().is_empty());
        assert!(replay.committed_txn_ids().is_empty());
    }

    #[test]
    fn record_accessors() {
        assert_eq!(put(1, 0).version(), Some(Version::new(1)));
        assert_eq!(WalRecord::Abort { txn_id: 4 }.version(), None);
        assert_eq!(WalRecord::Abort { txn_id: 4 }.txn_id(), Some(4));
        let cp = WalRecord::Checkpoint {
            version: Version::new(3),
        };
        assert_eq!(cp.txn_id(), None);
        assert_eq!(cp.version(), Some(Version::new(3)));
        assert!(!cp.is_data());
        assert!(put(1, 0).is_data());
    }

    #[test]
    fn segments_roll_at_the_configured_size() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = Wal::open(
            WalOptions::new(dir.path())
                .with_max_segment_bytes(256)
                .with_sync_on_commit(false),
        )
        .unwrap();
        for i in 0..40 {
            wal.append(&put(1, i)).unwrap();
        }
        wal.sync().unwrap();
        let paths = wal.segment_paths();
        assert!(paths.len() > 3, "expected several segments, got {paths:?}");
        // Names are the first LSN of each segment and are strictly increasing.
        let mut names: Vec<String> = paths
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        let sorted = {
            let mut n = names.clone();
            n.sort();
            n
        };
        assert_eq!(names, sorted, "segment names must sort in LSN order");
        names.dedup();
        assert_eq!(names.len(), paths.len());
        drop(wal);

        let replay = Wal::replay(dir.path()).unwrap();
        assert_eq!(replay.truncated_at, None);
        assert_eq!(replay.segments_read, paths.len());
        assert_eq!(replay.records.len(), 40);
        // Everything comes back in LSN order, across segment boundaries.
        for (i, (lsn, rec)) in replay.records.iter().enumerate() {
            assert_eq!(*lsn, Lsn::new(i as u64));
            assert_eq!(*rec, put(1, i as i64));
        }
    }

    #[test]
    fn a_record_larger_than_the_segment_size_still_gets_its_own_segment() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = Wal::open(WalOptions::new(dir.path()).with_max_segment_bytes(1)).unwrap();
        wal.append(&put(1, 1)).unwrap();
        wal.append(&put(1, 2)).unwrap();
        wal.sync().unwrap();
        assert_eq!(wal.segment_paths().len(), 2);
        drop(wal);
        let replay = Wal::replay(dir.path()).unwrap();
        assert_eq!(replay.records.len(), 2);
        assert_eq!(replay.truncated_at, None);
    }

    #[test]
    fn gc_removes_only_fully_superseded_segments() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = Wal::open(WalOptions::new(dir.path()).with_max_segment_bytes(1)).unwrap();
        // One record per segment: versions 1..=5 in segments 0..=4.
        for v in 1..=5u64 {
            wal.append(&WalRecord::Commit {
                txn_id: v,
                version: Version::new(v),
            })
            .unwrap();
        }
        wal.sync().unwrap();
        assert_eq!(wal.segment_paths().len(), 5);

        // Checkpoint at v3 supersedes the first three segments only.
        let removed = wal.gc(Version::new(3)).unwrap();
        assert_eq!(removed, 3);
        assert_eq!(wal.segment_paths().len(), 2);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);

        // Idempotent: nothing more qualifies at the same checkpoint.
        assert_eq!(wal.gc(Version::new(3)).unwrap(), 0);

        // The active segment is never removed, even far past the checkpoint.
        assert_eq!(wal.gc(Version::new(99)).unwrap(), 1);
        assert_eq!(wal.segment_paths().len(), 1);

        // Replay after GC returns exactly the surviving records.
        let replay = Wal::replay(dir.path()).unwrap();
        assert_eq!(replay.truncated_at, None);
        assert_eq!(replay.records.len(), 1);
        assert_eq!(replay.records[0].0, Lsn::new(4));
        assert_eq!(
            replay.records[0].1,
            WalRecord::Commit {
                txn_id: 5,
                version: Version::new(5),
            }
        );

        // GC advanced the oldest recoverable LSN.
        assert_eq!(wal.oldest_lsn(), Some(Lsn::new(4)));
        assert_eq!(wal.segment_first_lsns(), vec![Lsn::new(4)]);

        // Appends after GC keep the LSN sequence going.
        assert_eq!(wal.append(&put(9, 1)).unwrap(), Lsn::new(5));
    }

    #[test]
    fn gc_never_punches_a_hole_in_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = Wal::open(WalOptions::new(dir.path()).with_max_segment_bytes(1)).unwrap();
        // Segment 0 holds a high version, segment 1 a low one.
        wal.append(&commit(1, 50)).unwrap();
        wal.append(&commit(2, 2)).unwrap();
        wal.append(&commit(3, 3)).unwrap();
        wal.sync().unwrap();
        assert_eq!(wal.segment_paths().len(), 3);

        // Segment 1 alone would qualify at v2, but segment 0 does not, so GC
        // stops at the prefix boundary rather than leaving a gap.
        assert_eq!(wal.gc(Version::new(2)).unwrap(), 0);
        assert_eq!(wal.segment_paths().len(), 3);
        assert_eq!(Wal::replay(dir.path()).unwrap().records.len(), 3);
    }

    #[test]
    fn checkpoint_and_last_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = Wal::open(WalOptions::new(dir.path())).unwrap();
        wal.append(&WalRecord::Checkpoint {
            version: Version::new(4),
        })
        .unwrap();
        wal.append(&WalRecord::Checkpoint {
            version: Version::new(9),
        })
        .unwrap();
        wal.sync().unwrap();
        drop(wal);
        let replay = Wal::replay(dir.path()).unwrap();
        assert_eq!(replay.last_checkpoint(), Some(Version::new(9)));

        let empty = tempfile::tempdir().unwrap();
        assert_eq!(Wal::replay(empty.path()).unwrap().last_checkpoint(), None);
    }

    #[test]
    fn options_builders() {
        let o = WalOptions::new("/tmp/x");
        assert_eq!(o.max_segment_bytes, DEFAULT_MAX_SEGMENT_BYTES);
        assert!(o.sync_on_commit);
        let o = o.with_max_segment_bytes(99).with_sync_on_commit(false);
        assert_eq!(o.max_segment_bytes, 99);
        assert!(!o.sync_on_commit);
        assert_eq!(o.dir, PathBuf::from("/tmp/x"));

        let dir = tempfile::tempdir().unwrap();
        let wal = Wal::open(WalOptions::new(dir.path())).unwrap();
        assert_eq!(wal.options().dir, dir.path());
    }

    #[test]
    fn append_commit_syncs_and_is_durable_without_close() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = Wal::open(WalOptions::new(dir.path())).unwrap();
        wal.append(&put(1, 1)).unwrap();
        wal.append_commit(&commit(1, 2)).unwrap();
        // Deliberately do NOT drop the Wal: the data must already be on disk.
        let replay = Wal::replay(dir.path()).unwrap();
        assert_eq!(replay.records.len(), 2);
        assert_eq!(replay.committed_records().len(), 1);
    }

    fn scan_frame_bytes(bytes: &[u8]) -> SegmentScan {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(segment_file_name(Lsn::ZERO));
        std::fs::write(&path, bytes).unwrap();
        read_segment(&path, Lsn::ZERO).unwrap()
    }

    #[test]
    fn wal_frame_golden_bytes() {
        let frame = encode_frame(&commit(7, 9)).unwrap();
        assert_eq!(&frame[..8], &[35, 0, 0, 0, 135, 62, 109, 27]);
    }

    #[test]
    fn wal_frame_bad_magic_and_oversized() {
        // Bare WAL frames have no magic; corrupting the length field is the equivalent header test.
        let mut bad_header = encode_frame(&commit(7, 9)).unwrap();
        bad_header[0..4].copy_from_slice(&0u32.to_le_bytes());
        let scan = scan_frame_bytes(&bad_header);
        assert_eq!(scan.stopped, Some(Lsn::ZERO));

        let mut oversized = Vec::new();
        oversized.extend_from_slice(&((MAX_PAYLOAD_BYTES + 1) as u32).to_le_bytes());
        oversized.extend_from_slice(&0u32.to_le_bytes());
        let scan = scan_frame_bytes(&oversized);
        assert_eq!(scan.stopped, Some(Lsn::ZERO));
    }

    #[test]
    fn wal_frame_bad_version_and_crc() {
        // Bare WAL frames have no version; an undecodable CRC-clean payload covers format drift.
        let payload = b"not-a-wal-record";
        let mut bad_format = Vec::new();
        bad_format.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bad_format.extend_from_slice(&crc32c::crc32c(payload).to_le_bytes());
        bad_format.extend_from_slice(payload);
        let scan = scan_frame_bytes(&bad_format);
        assert_eq!(scan.stopped, Some(Lsn::ZERO));

        let mut bad_crc = encode_frame(&commit(7, 9)).unwrap();
        bad_crc[4] ^= 0xff;
        let scan = scan_frame_bytes(&bad_crc);
        assert_eq!(scan.stopped, Some(Lsn::ZERO));
    }

    #[test]
    fn wal_frame_size_check_truncated() {
        let mut frame = encode_frame(&commit(7, 9)).unwrap();
        frame.pop();
        let scan = scan_frame_bytes(&frame);
        assert_eq!(scan.stopped, Some(Lsn::ZERO));
    }

    #[test]
    fn wal_frame_size_check_trailing() {
        let mut frame = encode_frame(&commit(7, 9)).unwrap();
        frame.push(0);
        let scan = scan_frame_bytes(&frame);
        assert_eq!(scan.records.len(), 1);
        assert_eq!(scan.stopped, Some(Lsn::new(1)));
    }
}
