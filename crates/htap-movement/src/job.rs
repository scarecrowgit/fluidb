//! Movement job model, serialization envelope, and durable local mover.
//!
//! # Scope & Concurrency Guarantees
//! - **Single-node & single-tablet scope**: All data movement operations in this crate
//!   target local tablet partitions on a single node. Cross-node tablet distribution,
//!   rebalancing, and multi-partition coordinated transactions are deferred to
//!   higher-level coordinator and SQL layers.
//! - **Semantic idempotency**: Jobs are uniquely identified by a caller-assigned `job_id`.
//!   Submitting the same job request multiple times is semantically idempotent:
//!   terminal jobs (Complete or Failed) immediately return the durable report or error
//!   without re-executing side effects, and Running jobs return the in-flight state
//!   so callers can resume from the last committed progress counters. Exactly-once
//!   execution across crashes is not claimed; callers must handle semantic deduplication.
//! - **Synchronous & mutex-serialized**: All transitions and persistent updates are
//!   guarded by an internal mutex and synchronously flushed via `fsync` (including
//!   the containing directory). Multi-process concurrent access is not supported.

use std::fmt;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use htap_catalog::{TableId, TabletId};
use htap_common::{HtapError, Result, Row, Version};
use serde::{Deserialize, Serialize};

/// Catalog file name inside `<movement_root>/jobs/<job-id>/`.
pub const JOB_FILE_NAME: &str = "JOB";
/// Temporary file name used for atomic two-phase write-sync-rename.
pub const JOB_TMP_FILE_NAME: &str = "JOB.tmp";
/// Fixed 8-byte header magic bytes identifying durable movement job envelopes.
pub const HEADER_MAGIC: &[u8; 8] = b"HTAPJOB1";
/// Supported job envelope binary format version.
pub const FORMAT_VERSION: u16 = 1;
/// Fixed header length (8 magic + 2 version + 4 payload_len + 4 crc32c = 18 bytes).
pub const HEADER_LEN: usize = 18;
/// Maximum allowed job JSON payload size (16 MiB) to guard against unbounded allocations.
pub const MAX_JOB_PAYLOAD_BYTES: u32 = 16 * 1024 * 1024;

/// Supported file formats for data movement (COPY IN / COPY OUT).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DataFormat {
    /// Delimiter-separated values (default comma-separated).
    Csv,
    /// Newline-delimited JSON objects.
    JsonLines,
}

impl fmt::Display for DataFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Csv => write!(f, "CSV"),
            Self::JsonLines => write!(f, "JSONLINES"),
        }
    }
}

/// Operation kind for a data movement job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MovementJobKind {
    /// Ingest data from external files into a tablet.
    Import,
    /// Export tablet rows into external files.
    Export,
    /// Clone tablet data to another replica or partition.
    Clone,
    /// Repair or reconcile tablet segments.
    Repair,
}

impl fmt::Display for MovementJobKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Import => write!(f, "IMPORT"),
            Self::Export => write!(f, "EXPORT"),
            Self::Clone => write!(f, "CLONE"),
            Self::Repair => write!(f, "REPAIR"),
        }
    }
}

/// Execution phase of a data movement job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MovementJobPhase {
    /// Job is currently executing or ready to resume from progress counters.
    Running,
    /// Job completed successfully with a terminal report.
    Complete,
    /// Job encountered an unrecoverable error.
    Failed,
}

impl fmt::Display for MovementJobPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Running => write!(f, "RUNNING"),
            Self::Complete => write!(f, "COMPLETE"),
            Self::Failed => write!(f, "FAILED"),
        }
    }
}

/// Alias for [`MovementJobPhase`] representing the durable job state.
pub type MovementJobState = MovementJobPhase;

/// Progress counters tracked throughout job execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct JobCounters {
    /// Number of records parsed/read from the source.
    pub records_read: u64,
    /// Number of records successfully committed into storage or target.
    pub records_committed: u64,
    /// Number of storage rows physically written.
    pub rows_written: u64,
    /// Number of malformed or skipped records.
    pub records_skipped: u64,
}

impl JobCounters {
    /// Create new zeroed counters.
    #[inline]
    pub const fn new() -> Self {
        Self {
            records_read: 0,
            records_committed: 0,
            rows_written: 0,
            records_skipped: 0,
        }
    }
}

/// Final completion report produced when a job reaches [`MovementJobPhase::Complete`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyReport {
    /// Job identifier.
    pub job_id: String,
    /// Total records read from the source.
    pub records_read: u64,
    /// Total records committed to destination.
    pub records_committed: u64,
    /// Total storage rows written.
    pub rows_written: u64,
    /// Total records skipped or rejected.
    pub records_skipped: u64,
    /// Execution duration in milliseconds.
    pub duration_ms: u64,
}

/// User options for initiating a COPY IN / COPY OUT data movement operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyOptions {
    /// Unique identifier for this data movement job.
    pub job_id: String,
    /// Target table identifier.
    pub table_id: TableId,
    /// Target tablet identifier.
    pub tablet_id: TabletId,
    /// Data format (CSV or JSONLines).
    pub format: DataFormat,
    /// Path to source or destination file.
    pub path: PathBuf,
    /// Maximum rows to commit in a single transaction batch.
    pub batch_rows: usize,
    /// Maximum allowable parsing/conversion errors before aborting.
    pub max_errors: usize,
    /// Delimiter byte for CSV format (default `,`).
    pub delimiter: u8,
    /// Whether the CSV file includes a header line (default `true`).
    ///
    /// When `true`, the first row of CSV input is interpreted as column headers and matched
    /// against schema column names (allowing columns to appear in any order).
    /// When `false`, the first row is treated as data; fields are interpreted strictly in
    /// schema declaration order, requiring exactly `schema.len()` fields per record.
    pub has_header: bool,
    /// Optional MVCC snapshot version to pin during export or clone.
    pub pinned_version: Option<Version>,
}

impl CopyOptions {
    /// Default batch row limit (1,000 rows).
    pub const DEFAULT_BATCH_ROWS: usize = 1_000;
    /// Maximum allowable batch row limit (100,000 rows).
    pub const MAX_BATCH_ROWS: usize = 100_000;
    /// Maximum allowable length of a `job_id` string.
    pub const MAX_JOB_ID_LEN: usize = 128;

    /// Construct a new bounded `CopyOptions` with sensible defaults.
    pub fn new(
        job_id: impl Into<String>,
        table_id: TableId,
        tablet_id: TabletId,
        format: DataFormat,
        path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            job_id: job_id.into(),
            table_id,
            tablet_id,
            format,
            path: path.into(),
            batch_rows: Self::DEFAULT_BATCH_ROWS,
            max_errors: 0,
            delimiter: b',',
            has_header: true,
            pinned_version: None,
        }
    }

    /// Set the batch row count.
    #[inline]
    pub fn with_batch_rows(mut self, batch_rows: usize) -> Self {
        self.batch_rows = batch_rows;
        self
    }

    /// Set the maximum allowed row errors.
    #[inline]
    pub fn with_max_errors(mut self, max_errors: usize) -> Self {
        self.max_errors = max_errors;
        self
    }

    /// Set the CSV delimiter byte.
    #[inline]
    pub fn with_delimiter(mut self, delimiter: u8) -> Self {
        self.delimiter = delimiter;
        self
    }

    /// Configure whether CSV input/output has a header line.
    ///
    /// For CSV import:
    /// - `true` (default): first row is parsed as column headers and mapped against schema names.
    /// - `false`: first row is treated as data, interpreted in schema declaration order,
    ///   requiring exactly `schema.len()` fields per record.
    #[inline]
    pub fn with_has_header(mut self, has_header: bool) -> Self {
        self.has_header = has_header;
        self
    }

    /// Pin an MVCC snapshot version.
    #[inline]
    pub fn with_pinned_version(mut self, version: Version) -> Self {
        self.pinned_version = Some(version);
        self
    }

    /// Validate all options and path constraints.
    pub fn validate(&self) -> Result<()> {
        validate_job_id(&self.job_id)?;

        if self.batch_rows == 0 {
            return Err(HtapError::InvalidArgument(
                "batch_rows must be greater than 0".into(),
            ));
        }
        if self.batch_rows > Self::MAX_BATCH_ROWS {
            return Err(HtapError::InvalidArgument(format!(
                "batch_rows {} exceeds maximum limit {}",
                self.batch_rows,
                Self::MAX_BATCH_ROWS
            )));
        }

        let path_str = self.path.to_string_lossy();
        if path_str.is_empty() {
            return Err(HtapError::InvalidArgument("path cannot be empty".into()));
        }
        if path_str.contains('\0') {
            return Err(HtapError::InvalidArgument(
                "path cannot contain null characters".into(),
            ));
        }
        if self.path.is_dir() {
            return Err(HtapError::InvalidArgument(
                "path points to an existing directory, expected a file path".into(),
            ));
        }

        Ok(())
    }
}

/// Immutable request descriptor defining a movement job.
///
/// Any attempt to re-start a job with the same `job_id` but a different
/// descriptor will be rejected with [`HtapError::Conflict`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MovementJobRequest {
    /// Unique identifier for the job.
    pub job_id: String,
    /// Job kind (Import, Export, Clone, Repair).
    pub kind: MovementJobKind,
    /// Target table identifier.
    pub table_id: TableId,
    /// Target tablet identifier.
    pub tablet_id: TabletId,
    /// Data serialization format.
    pub format: DataFormat,
    /// Path to data file.
    pub path: PathBuf,
    /// Batch commit size.
    pub batch_rows: usize,
    /// Tolerated error count.
    pub max_errors: usize,
    /// Delimiter byte (for CSV).
    pub delimiter: u8,
    /// Header flag (for CSV).
    pub has_header: bool,
    /// Optional pinned snapshot version.
    pub pinned_version: Option<Version>,
}

impl MovementJobRequest {
    /// Construct a job request from a kind and [`CopyOptions`].
    pub fn from_copy_options(kind: MovementJobKind, options: &CopyOptions) -> Self {
        Self {
            job_id: options.job_id.clone(),
            kind,
            table_id: options.table_id,
            tablet_id: options.tablet_id,
            format: options.format,
            path: options.path.clone(),
            batch_rows: options.batch_rows,
            max_errors: options.max_errors,
            delimiter: options.delimiter,
            has_header: options.has_header,
            pinned_version: options.pinned_version,
        }
    }

    /// Validate the request descriptor parameters.
    pub fn validate(&self) -> Result<()> {
        validate_job_id(&self.job_id)?;

        if self.batch_rows == 0 {
            return Err(HtapError::InvalidArgument(
                "batch_rows must be greater than 0".into(),
            ));
        }
        if self.batch_rows > CopyOptions::MAX_BATCH_ROWS {
            return Err(HtapError::InvalidArgument(format!(
                "batch_rows {} exceeds maximum limit {}",
                self.batch_rows,
                CopyOptions::MAX_BATCH_ROWS
            )));
        }

        let path_str = self.path.to_string_lossy();
        if path_str.is_empty() {
            return Err(HtapError::InvalidArgument("path cannot be empty".into()));
        }
        if path_str.contains('\0') {
            return Err(HtapError::InvalidArgument(
                "path cannot contain null characters".into(),
            ));
        }
        if self.path.is_dir() {
            return Err(HtapError::InvalidArgument(
                "path points to an existing directory, expected a file path".into(),
            ));
        }

        Ok(())
    }
}

impl From<&MovementJobRequest> for MovementJobRequest {
    fn from(req: &MovementJobRequest) -> Self {
        req.clone()
    }
}

/// Durable movement job entity tracking request specification and state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MovementJob {
    /// Immutable request descriptor.
    pub request: MovementJobRequest,
    /// Current execution phase.
    pub phase: MovementJobPhase,
    /// Target table identifier.
    pub table_id: TableId,
    /// Target tablet identifier.
    pub tablet_id: TabletId,
    /// Target data format.
    pub format: DataFormat,
    /// Incurred progress counters.
    pub counters: JobCounters,
    /// Optional progress checkpoint token or byte offset.
    pub checkpoint: Option<String>,
    /// Optional pinned MVCC snapshot version.
    pub pinned_version: Option<Version>,
    /// Terminal report populated upon successful completion.
    pub report: Option<CopyReport>,
    /// Terminal failure description if the job failed.
    pub error: Option<String>,
}

impl MovementJob {
    /// Create a new running job from an immutable request descriptor.
    pub fn new(request: MovementJobRequest) -> Self {
        Self {
            table_id: request.table_id,
            tablet_id: request.tablet_id,
            format: request.format,
            pinned_version: request.pinned_version,
            counters: JobCounters::new(),
            checkpoint: None,
            report: None,
            error: None,
            phase: MovementJobPhase::Running,
            request,
        }
    }

    /// Return the unique job ID.
    #[inline]
    pub fn job_id(&self) -> &str {
        &self.request.job_id
    }

    /// Return the job kind.
    #[inline]
    pub fn kind(&self) -> MovementJobKind {
        self.request.kind
    }

    /// Return the current execution phase.
    #[inline]
    pub fn phase(&self) -> MovementJobPhase {
        self.phase
    }

    /// Return the durable job state (synonym for `phase`).
    #[inline]
    pub fn state(&self) -> MovementJobState {
        self.phase
    }

    /// Return whether this job is currently in progress.
    #[inline]
    pub fn is_running(&self) -> bool {
        self.phase == MovementJobPhase::Running
    }

    /// Return whether this job completed successfully.
    #[inline]
    pub fn is_complete(&self) -> bool {
        self.phase == MovementJobPhase::Complete
    }

    /// Return whether this job failed.
    #[inline]
    pub fn is_failed(&self) -> bool {
        self.phase == MovementJobPhase::Failed
    }

    /// Return whether this job is in a terminal phase (Complete or Failed).
    #[inline]
    pub fn is_terminal(&self) -> bool {
        self.phase != MovementJobPhase::Running
    }

    /// Validate job internal invariants.
    pub fn validate(&self) -> Result<()> {
        self.request.validate()?;

        if self.table_id != self.request.table_id {
            return Err(HtapError::Corruption(format!(
                "job table_id ({}) does not match request table_id ({})",
                self.table_id.0, self.request.table_id.0
            )));
        }
        if self.tablet_id != self.request.tablet_id {
            return Err(HtapError::Corruption(format!(
                "job tablet_id ({}) does not match request tablet_id ({})",
                self.tablet_id.0, self.request.tablet_id.0
            )));
        }
        if self.format != self.request.format {
            return Err(HtapError::Corruption(format!(
                "job format ({:?}) does not match request format ({:?})",
                self.format, self.request.format
            )));
        }
        if self.pinned_version != self.request.pinned_version {
            return Err(HtapError::Corruption(
                "job pinned_version does not match request pinned_version".into(),
            ));
        }

        match self.phase {
            MovementJobPhase::Running => {
                if self.report.is_some() {
                    return Err(HtapError::Corruption(
                        "running job must not contain a completion report".into(),
                    ));
                }
                if self.error.is_some() {
                    return Err(HtapError::Corruption(
                        "running job must not contain a failure error".into(),
                    ));
                }
            }
            MovementJobPhase::Complete => {
                if self.report.is_none() {
                    return Err(HtapError::Corruption(
                        "completed job must contain a completion report".into(),
                    ));
                }
            }
            MovementJobPhase::Failed => {
                if self.error.is_none() {
                    return Err(HtapError::Corruption(
                        "failed job must contain a failure error description".into(),
                    ));
                }
            }
        }

        Ok(())
    }

    /// Produce a completion report from the current job state.
    pub fn to_report(&self, duration_ms: u64) -> CopyReport {
        if let Some(ref rep) = self.report {
            return rep.clone();
        }
        CopyReport {
            job_id: self.request.job_id.clone(),
            records_read: self.counters.records_read,
            records_committed: self.counters.records_committed,
            rows_written: self.counters.rows_written,
            records_skipped: self.counters.records_skipped,
            duration_ms,
        }
    }
}

/// Validate that a `job_id` is well-formed and does not attempt path traversal.
pub fn validate_job_id(job_id: &str) -> Result<()> {
    if job_id.is_empty() {
        return Err(HtapError::InvalidArgument("job_id cannot be empty".into()));
    }
    if job_id.len() > CopyOptions::MAX_JOB_ID_LEN {
        return Err(HtapError::InvalidArgument(format!(
            "job_id length {} exceeds maximum allowed {}",
            job_id.len(),
            CopyOptions::MAX_JOB_ID_LEN
        )));
    }
    if job_id == "." || job_id == ".." {
        return Err(HtapError::InvalidArgument(format!(
            "job_id '{job_id}' is a reserved path traversal token"
        )));
    }
    if job_id.contains('/') || job_id.contains('\\') || job_id.contains('\0') {
        return Err(HtapError::InvalidArgument(format!(
            "job_id '{job_id}' contains invalid path separator or null byte"
        )));
    }
    if job_id.contains("..") {
        return Err(HtapError::InvalidArgument(format!(
            "job_id '{job_id}' contains forbidden path traversal token '..'"
        )));
    }
    if job_id.starts_with('.') || job_id.starts_with('-') {
        return Err(HtapError::InvalidArgument(format!(
            "job_id '{job_id}' cannot start with '.' or '-'"
        )));
    }
    // Only allow alphanumeric characters, underscores, hyphens, and dots.
    if !job_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        return Err(HtapError::InvalidArgument(format!(
            "job_id '{job_id}' contains disallowed characters"
        )));
    }

    Ok(())
}

/// Encode a movement job entity into a versioned, CRC32C-checked binary envelope.
pub fn encode_job(job: &MovementJob) -> Result<Vec<u8>> {
    let payload = serde_json::to_vec(job)
        .map_err(|e| HtapError::Internal(format!("failed to serialize movement job: {e}")))?;

    if payload.len() > MAX_JOB_PAYLOAD_BYTES as usize {
        return Err(HtapError::InvalidArgument(format!(
            "job payload size {} exceeds maximum allowed limit {}",
            payload.len(),
            MAX_JOB_PAYLOAD_BYTES
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

/// Decode and validate a movement job entity from a versioned binary envelope.
pub fn decode_job(bytes: &[u8]) -> Result<MovementJob> {
    if bytes.len() < HEADER_LEN {
        return Err(HtapError::Corruption(format!(
            "job file too small: {} bytes, minimum header length is {}",
            bytes.len(),
            HEADER_LEN
        )));
    }

    if &bytes[0..8] != HEADER_MAGIC {
        return Err(HtapError::Corruption(
            "invalid movement job header magic bytes".into(),
        ));
    }

    let version = u16::from_le_bytes(bytes[8..10].try_into().unwrap());
    if version != FORMAT_VERSION {
        return Err(HtapError::Corruption(format!(
            "unsupported movement job format version: {version}"
        )));
    }

    let payload_len = u32::from_le_bytes(bytes[10..14].try_into().unwrap());
    let expected_crc = u32::from_le_bytes(bytes[14..18].try_into().unwrap());

    if payload_len > MAX_JOB_PAYLOAD_BYTES {
        return Err(HtapError::Corruption(format!(
            "job payload length {} exceeds maximum limit {}",
            payload_len, MAX_JOB_PAYLOAD_BYTES
        )));
    }

    let expected_total = HEADER_LEN + payload_len as usize;
    if bytes.len() < expected_total {
        return Err(HtapError::Corruption(format!(
            "truncated movement job file: expected {} bytes, found {}",
            expected_total,
            bytes.len()
        )));
    }

    if bytes.len() > expected_total {
        return Err(HtapError::Corruption(format!(
            "job file has {} trailing leftover bytes",
            bytes.len() - expected_total
        )));
    }

    let payload = &bytes[HEADER_LEN..expected_total];
    let computed_crc = crc32c::crc32c(payload);
    if computed_crc != expected_crc {
        return Err(HtapError::Corruption(format!(
            "job checksum mismatch: expected {expected_crc:#010x}, got {computed_crc:#010x}"
        )));
    }

    let job: MovementJob = serde_json::from_slice(payload).map_err(|e| {
        HtapError::Corruption(format!("failed to deserialize movement job JSON: {e}"))
    })?;

    job.validate()?;

    Ok(job)
}

/// Durable fsync on directory metadata.
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

/// Local synchronous data mover managing single-tablet data movement jobs.
///
/// # Concurrency & Consistency Contract
/// - **Single-Node / Single-Tablet**: Manages data movement for local tablet partitions.
///   Distributed routing, replica rebalancing, and multi-partition coordination
///   are deferred to higher layers.
/// - **Semantic Idempotency**: Repeated calls to start/resume with identical request
///   descriptors succeed idempotently without side effects. Terminal jobs return their
///   saved completion report or failure message. Running jobs return the in-flight
///   checkpoint state from which progress resumes. Exactly-once execution across crashes
///   is not guaranteed.
/// - **Synchronous & Serialized**: All job transitions and state mutations are guarded
///   by an internal mutex and synchronously committed to disk via atomic rename and fsync.
///   Multi-process concurrency is not supported.
#[derive(Debug)]
pub struct LocalDataMover {
    root_dir: PathBuf,
    lock: parking_lot::Mutex<()>,
}

impl LocalDataMover {
    /// Open or create a local data mover repository at `<movement_root>`.
    pub fn new(root_dir: impl Into<PathBuf>) -> Result<Self> {
        let root_dir = root_dir.into();
        fs::create_dir_all(&root_dir)?;
        fs::create_dir_all(root_dir.join("jobs"))?;

        Ok(Self {
            root_dir,
            lock: parking_lot::Mutex::new(()),
        })
    }

    /// Return the root directory of this data mover.
    #[inline]
    pub fn root_dir(&self) -> &Path {
        &self.root_dir
    }

    /// Return the jobs directory `<movement_root>/jobs`.
    #[inline]
    pub fn jobs_dir(&self) -> PathBuf {
        self.root_dir.join("jobs")
    }

    /// Return the directory for a specific job: `<movement_root>/jobs/<job-id>`.
    pub fn job_dir(&self, job_id: &str) -> PathBuf {
        self.jobs_dir().join(job_id)
    }

    /// Return the persistent envelope file path: `<movement_root>/jobs/<job-id>/JOB`.
    pub fn job_file_path(&self, job_id: &str) -> PathBuf {
        self.job_dir(job_id).join(JOB_FILE_NAME)
    }

    /// Return the temporary envelope file path: `<movement_root>/jobs/<job-id>/JOB.tmp`.
    pub fn job_tmp_file_path(&self, job_id: &str) -> PathBuf {
        self.job_dir(job_id).join(JOB_TMP_FILE_NAME)
    }

    /// Start a new movement job, or idempotently return the existing job if already registered.
    ///
    /// # Error Semantics
    /// - Returns [`HtapError::Conflict`] if a job with the same `job_id` exists but
    ///   has a different immutable request descriptor.
    /// - Returns [`HtapError::InvalidArgument`] if the request parameters or `job_id` are invalid.
    pub fn start_job(&self, request: impl Into<MovementJobRequest>) -> Result<MovementJob> {
        let request = request.into();
        request.validate()?;

        let _guard = self.lock.lock();

        if let Some(existing) = self.read_job_file_locked(&request.job_id)? {
            if existing.request != request {
                return Err(HtapError::Conflict(format!(
                    "job '{}' already exists with a different request descriptor",
                    request.job_id
                )));
            }
            // Idempotent retry: returns saved report/job without re-executing
            return Ok(existing);
        }

        let job = MovementJob::new(request);
        self.persist_job_locked(&job)?;

        Ok(job)
    }

    /// Start a data movement copy operation from [`CopyOptions`].
    pub fn start_copy(&self, kind: MovementJobKind, options: &CopyOptions) -> Result<MovementJob> {
        options.validate()?;
        let req = MovementJobRequest::from_copy_options(kind, options);
        self.start_job(req)
    }

    /// Resume an existing job by `job_id`.
    ///
    /// Returns [`HtapError::NotFound`] if no job exists with the given ID.
    pub fn resume_job(&self, job_id: &str) -> Result<MovementJob> {
        validate_job_id(job_id)?;
        let _guard = self.lock.lock();

        match self.read_job_file_locked(job_id)? {
            Some(job) => Ok(job),
            None => Err(HtapError::NotFound(format!("job '{job_id}' not found"))),
        }
    }

    /// Load an existing job by `job_id` if present, returning `None` if it does not exist.
    pub fn load_job(&self, job_id: &str) -> Result<Option<MovementJob>> {
        validate_job_id(job_id)?;
        let _guard = self.lock.lock();

        self.read_job_file_locked(job_id)
    }

    /// Commit progress counters and optional checkpoint token for a running job.
    ///
    /// Progress is persisted only after the caller has successfully committed the batch.
    ///
    /// # Error Semantics
    /// - Returns [`HtapError::NotFound`] if the job does not exist.
    /// - Returns [`HtapError::Conflict`] if the job has already reached a terminal phase.
    pub fn commit_progress(
        &self,
        job_id: &str,
        counters: JobCounters,
        checkpoint: Option<String>,
    ) -> Result<MovementJob> {
        validate_job_id(job_id)?;
        let _guard = self.lock.lock();

        let mut job = self
            .read_job_file_locked(job_id)?
            .ok_or_else(|| HtapError::NotFound(format!("job '{job_id}' not found")))?;

        if job.is_terminal() {
            return Err(HtapError::Conflict(format!(
                "cannot commit progress on terminal job '{job_id}' in phase {:?}",
                job.phase
            )));
        }

        job.counters = counters;
        job.checkpoint = checkpoint;
        self.persist_job_locked(&job)?;

        Ok(job)
    }

    /// Mark a job as successfully completed with its final [`CopyReport`].
    ///
    /// If the job was already completed, this operation is a no-op returning the saved job.
    ///
    /// # Error Semantics
    /// - Returns [`HtapError::NotFound`] if the job does not exist.
    /// - Returns [`HtapError::Conflict`] if the job is in a Failed state.
    pub fn complete_job(&self, job_id: &str, report: CopyReport) -> Result<MovementJob> {
        validate_job_id(job_id)?;
        let _guard = self.lock.lock();

        let mut job = self
            .read_job_file_locked(job_id)?
            .ok_or_else(|| HtapError::NotFound(format!("job '{job_id}' not found")))?;

        if job.phase == MovementJobPhase::Complete {
            return Ok(job);
        }
        if job.phase == MovementJobPhase::Failed {
            return Err(HtapError::Conflict(format!(
                "cannot mark failed job '{job_id}' as complete"
            )));
        }

        job.phase = MovementJobPhase::Complete;
        job.counters.records_read = report.records_read;
        job.counters.records_committed = report.records_committed;
        job.counters.rows_written = report.rows_written;
        job.counters.records_skipped = report.records_skipped;
        job.report = Some(report);
        self.persist_job_locked(&job)?;

        Ok(job)
    }

    /// Mark a running job as failed with an unrecoverable error description.
    ///
    /// If the job was already marked failed, this operation is an idempotent no-op.
    ///
    /// # Error Semantics
    /// - Returns [`HtapError::NotFound`] if the job does not exist.
    /// - Returns [`HtapError::Conflict`] if the job has already successfully completed.
    pub fn fail_job(&self, job_id: &str, error: impl Into<String>) -> Result<MovementJob> {
        validate_job_id(job_id)?;
        let _guard = self.lock.lock();

        let mut job = self
            .read_job_file_locked(job_id)?
            .ok_or_else(|| HtapError::NotFound(format!("job '{job_id}' not found")))?;

        if job.phase == MovementJobPhase::Failed {
            return Ok(job);
        }
        if job.phase == MovementJobPhase::Complete {
            return Err(HtapError::Conflict(format!(
                "cannot mark already completed job '{job_id}' as failed"
            )));
        }

        job.phase = MovementJobPhase::Failed;
        job.error = Some(error.into());
        self.persist_job_locked(&job)?;

        Ok(job)
    }

    /// Import records from a generic CSV [`std::io::Read`] stream.
    ///
    /// # Consistency & Crash Recovery
    /// Mutations are batched and committed via [`htap_txn::TransactionManager::commit_request`]
    /// with participant ID 1 before job progress counters are checkpointed. If a crash
    /// occurs after the transaction commits but before the job counter checkpoint is written,
    /// a commit-before-checkpoint replay window exists. On recovery/resume, uncheckpointed
    /// records are replayed. Because rows are upserted by primary key (`Mutation::Put`),
    /// replay is idempotent, but exactly-once execution across crashes is not claimed.
    ///
    /// # Resume
    /// Resuming an in-flight job using a non-seekable stream returns [`HtapError::Unsupported`].
    pub fn copy_from_csv_reader<R: std::io::Read>(
        &self,
        options: &CopyOptions,
        catalog: &dyn htap_catalog::store::CatalogStore,
        txn_manager: &htap_txn::TransactionManager,
        reader: R,
    ) -> Result<CopyReport> {
        crate::import::copy_from_csv_reader(self, options, catalog, txn_manager, reader)
    }

    /// Import records from a generic JSONLines [`std::io::Read`] stream.
    ///
    /// # Resume
    /// Resuming an in-flight job using a non-seekable stream returns [`HtapError::Unsupported`].
    pub fn copy_from_jsonl_reader<R: std::io::Read>(
        &self,
        options: &CopyOptions,
        catalog: &dyn htap_catalog::store::CatalogStore,
        txn_manager: &htap_txn::TransactionManager,
        reader: R,
    ) -> Result<CopyReport> {
        crate::import::copy_from_jsonl_reader(self, options, catalog, txn_manager, reader)
    }

    /// Import records from a CSV file at `options.path`.
    ///
    /// On resume, reopens the source file and discards exactly the checkpointed logical records.
    pub fn copy_from_csv(
        &self,
        options: &CopyOptions,
        catalog: &dyn htap_catalog::store::CatalogStore,
        txn_manager: &htap_txn::TransactionManager,
    ) -> Result<CopyReport> {
        crate::import::copy_from_csv(self, options, catalog, txn_manager)
    }

    /// Import records from a JSONLines file at `options.path`.
    ///
    /// On resume, reopens the source file and discards exactly the checkpointed logical records.
    pub fn copy_from_jsonl(
        &self,
        options: &CopyOptions,
        catalog: &dyn htap_catalog::store::CatalogStore,
        txn_manager: &htap_txn::TransactionManager,
    ) -> Result<CopyReport> {
        crate::import::copy_from_jsonl(self, options, catalog, txn_manager)
    }

    /// Import records from the file specified in `options.path` according to `options.format`.
    pub fn import(
        &self,
        options: &CopyOptions,
        catalog: &dyn htap_catalog::store::CatalogStore,
        txn_manager: &htap_txn::TransactionManager,
    ) -> Result<CopyReport> {
        crate::import::import(self, options, catalog, txn_manager)
    }

    /// Export partition records to a generic CSV [`std::io::Write`] stream.
    ///
    /// Pins the engine snapshot once, collapses MVCC/tombstones, and outputs rows in
    /// deterministic encoded-PK order.
    pub fn copy_to_csv_writer<W: std::io::Write>(
        &self,
        options: &CopyOptions,
        catalog: &dyn htap_catalog::store::CatalogStore,
        engine: &htap_rowstore::Engine,
        writer: W,
    ) -> Result<CopyReport> {
        crate::export::copy_to_csv_writer(self, options, catalog, engine, writer)
    }

    /// Export partition records to a generic JSONLines [`std::io::Write`] stream.
    ///
    /// Pins the engine snapshot once, collapses MVCC/tombstones, and outputs rows in
    /// deterministic encoded-PK order.
    pub fn copy_to_jsonl_writer<W: std::io::Write>(
        &self,
        options: &CopyOptions,
        catalog: &dyn htap_catalog::store::CatalogStore,
        engine: &htap_rowstore::Engine,
        writer: W,
    ) -> Result<CopyReport> {
        crate::export::copy_to_jsonl_writer(self, options, catalog, engine, writer)
    }

    /// Export partition records to a CSV file at `options.path`.
    ///
    /// Output file is written atomically via temporary file, fsync, and rename.
    pub fn copy_to_csv(
        &self,
        options: &CopyOptions,
        catalog: &dyn htap_catalog::store::CatalogStore,
        engine: &htap_rowstore::Engine,
    ) -> Result<CopyReport> {
        crate::export::copy_to_csv(self, options, catalog, engine)
    }

    /// Export partition records to a JSONLines file at `options.path`.
    ///
    /// Output file is written atomically via temporary file, fsync, and rename.
    pub fn copy_to_jsonl(
        &self,
        options: &CopyOptions,
        catalog: &dyn htap_catalog::store::CatalogStore,
        engine: &htap_rowstore::Engine,
    ) -> Result<CopyReport> {
        crate::export::copy_to_jsonl(self, options, catalog, engine)
    }

    /// Export partition records to the file specified in `options.path` according to `options.format`.
    pub fn export(
        &self,
        options: &CopyOptions,
        catalog: &dyn htap_catalog::store::CatalogStore,
        engine: &htap_rowstore::Engine,
    ) -> Result<CopyReport> {
        crate::export::export(self, options, catalog, engine)
    }

    /// Decode a raw data record using a table schema.
    pub fn decode_record_with_schema(
        &self,
        schema: &htap_common::Schema,
        format: DataFormat,
        data: &[u8],
    ) -> Result<Row> {
        crate::codec::decode_record(schema, format, data)
    }

    /// Encode a row into raw bytes using a table schema.
    pub fn encode_record_with_schema(
        &self,
        schema: &htap_common::Schema,
        format: DataFormat,
        row: &Row,
    ) -> Result<Vec<u8>> {
        crate::codec::encode_record(schema, format, row)
    }

    /// Codec decode stub (use [`LocalDataMover::decode_record_with_schema`] for schema-aware decoding).
    pub fn decode_record(&self, format: DataFormat, data: &[u8]) -> Result<Row> {
        let _ = (format, data);
        Err(HtapError::Unsupported(
            "schema-less decode is unsupported; use decode_record_with_schema".into(),
        ))
    }

    /// Codec encode stub (use [`LocalDataMover::encode_record_with_schema`] for schema-aware encoding).
    pub fn encode_record(&self, format: DataFormat, row: &Row) -> Result<Vec<u8>> {
        let _ = (format, row);
        Err(HtapError::Unsupported(
            "schema-less encode is unsupported; use encode_record_with_schema".into(),
        ))
    }

    fn read_job_file_locked(&self, job_id: &str) -> Result<Option<MovementJob>> {
        let path = self.job_file_path(job_id);
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(HtapError::Io(e)),
        };

        let job = decode_job(&bytes)?;
        if job.request.job_id != job_id {
            return Err(HtapError::Corruption(format!(
                "job file inside '{job_id}' directory has mismatched job_id '{}'",
                job.request.job_id
            )));
        }

        Ok(Some(job))
    }

    fn persist_job_locked(&self, job: &MovementJob) -> Result<()> {
        let job_dir = self.job_dir(&job.request.job_id);
        fs::create_dir_all(&job_dir)?;

        let tmp_path = self.job_tmp_file_path(&job.request.job_id);
        let final_path = self.job_file_path(&job.request.job_id);

        let encoded = encode_job(job)?;

        // 1. Write tmp file and fsync
        let write_res = (|| -> Result<()> {
            let mut file = File::create(&tmp_path)?;
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

        // 3. Fsync job directory and jobs directory
        sync_dir(&job_dir)?;
        let _ = sync_dir(&self.jobs_dir());

        Ok(())
    }
}
