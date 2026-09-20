//! Shared types: errors, config, MVCC version domain, the relational data
//! model and the order-preserving key encoder.
//!
//! Every public item of the submodules is re-exported here, so downstream
//! crates keep using `htap_common::Version`, `htap_common::HtapError`, etc.
//!
//! Modules:
//! - [`error`]: [`HtapError`] and the crate [`Result`] alias.
//! - [`version`]: [`Version`] (MVCC) and [`FencingToken`] (leadership).
//! - [`types`]: [`DataType`], [`Value`], [`ColumnDef`], [`Schema`], [`Row`].
//! - [`keycodec`]: order-preserving composite key encoding.
//! - [`fs`]: bounded I/O and durable atomic publication.
//! - [`envelope`]: versioned CRC32-C envelope framing.
//! - [`bytecursor`]: checked little-endian byte reads.
//!
//! ```
//! use htap_common::{FencingToken, HtapError, Result, Value, Version};
//!
//! fn bump(v: Version, t: FencingToken, last: FencingToken) -> Result<Version> {
//!     if t < last {
//!         return Err(HtapError::Fenced { expected: last.get(), got: t.get() });
//!     }
//!     v.checked_next()
//! }
//!
//! let v = bump(Version::INITIAL, FencingToken::new(2), FencingToken::INITIAL)?;
//! assert_eq!(v, Version::new(2));
//!
//! // Byte order of an encoded key matches the logical order of its values.
//! let lo = htap_common::encode_key(&[Value::Int64(-1)])?;
//! let hi = htap_common::encode_key(&[Value::Int64(1)])?;
//! assert!(lo < hi);
//! # Ok::<(), HtapError>(())
//! ```
#![forbid(unsafe_code)]

pub mod bytecursor;
pub mod envelope;
pub mod error;
pub mod fs;
pub mod keycodec;
pub mod lock;
pub mod password;
pub mod types;
pub mod version;

pub use error::{HtapError, Result};
pub use fs::read_file_exact_bounded;
pub use keycodec::{encode_key, encode_key_prefix};
pub use lock::{ProcessLock, LOCK_FILE_NAME};
pub use password::{hash_native_password, scramble_native_password, verify_native_password_hash};
pub use types::{ColumnDef, DataType, Mutation, Row, Schema, Value};
pub use version::{FencingToken, Version};
