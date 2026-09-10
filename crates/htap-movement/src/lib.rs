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

pub mod codec;
pub mod export;
pub mod import;
pub mod job;
pub mod tablet;

pub use codec::{
    decode_csv_record, decode_hex, decode_json_line, decode_record, encode_csv_header,
    encode_csv_record, encode_hex, encode_json_line, encode_record, format_csv_field,
    format_json_value, parse_csv_field, parse_json_value, CsvHeaderMap, MAX_FIELD_BYTES,
    MAX_LINE_BYTES,
};
pub use export::{
    collapse_entries_to_rows, copy_to_csv, copy_to_csv_writer, copy_to_jsonl, copy_to_jsonl_writer,
    export,
};
pub use import::{
    copy_from_csv, copy_from_csv_reader, copy_from_jsonl, copy_from_jsonl_reader, import,
    resolve_table_topology, ResolvedTopology,
};
pub use job::{
    decode_job, encode_job, sync_dir, validate_job_id, CopyOptions, CopyReport, DataFormat,
    JobCounters, LocalDataMover, MovementJob, MovementJobKind, MovementJobPhase,
    MovementJobRequest, MovementJobState, FORMAT_VERSION, HEADER_LEN, HEADER_MAGIC, JOB_FILE_NAME,
    JOB_TMP_FILE_NAME, MAX_JOB_PAYLOAD_BYTES,
};
pub use tablet::{
    clone_tablet, decode_manifest, encode_manifest, repair_tablet, verify_package,
    TabletCloneOptions, TabletPackageManifest, MANIFEST_FORMAT_VERSION, MANIFEST_HEADER_LEN,
    MANIFEST_HEADER_MAGIC, MAX_MANIFEST_PAYLOAD_BYTES, MAX_PACKAGE_DATA_BYTES,
};
