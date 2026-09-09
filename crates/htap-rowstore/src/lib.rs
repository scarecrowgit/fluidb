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

pub mod memtable;
pub mod wal;

pub use memtable::{InternalKey, Memtable, MemtableEntry, ValueKind};
pub use wal::{Lsn, Wal, WalOptions, WalRecord, WalReplay};
