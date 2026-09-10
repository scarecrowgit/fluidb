//! LSM row store (WAL, memtable, SST, PK index).
//!
//! See [`wal`] for write-ahead logging and [`memtable`] for the in-memory
//! MVCC write buffer.
//!
//! ```
//! use htap_common::{Row, Value, Version};
//! use htap_rowstore::{Wal, WalOptions, WalRecord};
//!
//! # fn main() -> htap_common::Result<()> {
//! let dir = tempfile::tempdir().unwrap();
//! let mut wal = Wal::open(WalOptions::new(dir.path()))?;
//!
//! wal.append(&WalRecord::Put {
//!     txn_id: 1,
//!     partition_id: 0,
//!     key: b"k1".to_vec(),
//!     row: Row::new(vec![Value::Int64(1)]),
//!     version: Version::new(2),
//! })?;
//! // Returning from `append_commit` means the transaction is durable.
//! wal.append_commit(&WalRecord::Commit { txn_id: 1, version: Version::new(2) })?;
//!
//! // After a crash, recovery replays only what actually committed.
//! let replay = Wal::replay(dir.path())?;
//! assert_eq!(replay.committed_records().len(), 1);
//! # Ok(())
//! # }
//! ```
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod engine;
pub mod manifest;
pub mod memtable;
pub mod sst;
pub mod wal;

pub use engine::{
    Engine, EngineOptions, Mutation, PreparedTransaction, Snapshot, DEFAULT_MEMTABLE_BYTES,
};
pub use manifest::{
    Manifest, ManifestLedgerEntry, ManifestSstEntry, FORMAT_VERSION, FORMAT_VERSION_V1,
    FORMAT_VERSION_V2, MAX_APPLIED_EXTERNAL_TXNS, MAX_MANIFEST_PAYLOAD_BYTES, MAX_SST_COUNT,
};
pub use memtable::{InternalKey, Memtable, MemtableEntry, ValueKind};
pub use sst::{SstMetadata, SstOptions, SstReader, SstWriter, DEFAULT_SST_BLOCK_BYTES};
pub use wal::{Lsn, Wal, WalOptions, WalRecord, WalReplay};
