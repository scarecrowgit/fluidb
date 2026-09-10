//! Load, export, streaming ingest, and tablet data movement.
//!
//! # Scope and Concurrency Guarantees
//! - **Single-Node & Single-Tablet Scope**: All data movement operations in this crate
//!   are local to a single node and operate against a single tablet at a time.
//!   Distributed multi-node coordinated migrations and cross-partition routing are
//!   deferred to higher-level coordination layers.
//! - **Semantic Idempotency**: Jobs are tracked durably on disk under a unique `job_id`.
//!   Re-starting or re-executing a job with the identical immutable request descriptor
//!   is idempotent: terminal jobs return their recorded [`CopyReport`] as a no-op
//!   without re-executing storage mutations, and in-flight jobs resume from the last
//!   committed batch counters. Exactly-once execution across crashes is not claimed;
//!   callers must ensure semantic idempotency at the storage layer.
//! - **Synchronous & Serialized**: State transitions and envelope updates are
//!   guarded by an internal mutex and synchronously committed with directory `fsync`.
//!   Cross-process concurrency is not supported.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod job;

pub use job::{
    decode_job, encode_job, sync_dir, validate_job_id, CopyOptions, CopyReport, DataFormat,
    JobCounters, LocalDataMover, MovementJob, MovementJobKind, MovementJobPhase,
    MovementJobRequest, MovementJobState, FORMAT_VERSION, HEADER_LEN, HEADER_MAGIC, JOB_FILE_NAME,
    JOB_TMP_FILE_NAME, MAX_JOB_PAYLOAD_BYTES,
};
