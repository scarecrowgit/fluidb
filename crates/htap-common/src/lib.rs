//! Shared types: errors, config, MVCC version domain
#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;

/// Monotonic MVCC version. Shared by the row store and column store so a
/// transaction can touch both. Version 1 is the empty/initial version;
/// the first write lands at version 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Version(u64);

impl Version {
    /// Initial empty version (version 1).
    pub const INITIAL: Self = Self(1);

    /// Create a new Version with the specified raw value.
    #[inline]
    pub const fn new(val: u64) -> Self {
        Self(val)
    }

    /// Return the raw u64 version number.
    #[inline]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Return the strictly next version.
    #[inline]
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}", self.0)
    }
}

/// Monotonically increasing fencing token handed out by the Coordinator on
/// leadership acquisition. A holder must present a token >= the last observed
/// token for a write to be accepted; this is what prevents a stale leader from
/// writing after a partition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FencingToken(u64);

impl FencingToken {
    /// Initial fencing token (value 1).
    pub const INITIAL: Self = Self(1);

    /// Create a new FencingToken with the specified raw value.
    #[inline]
    pub const fn new(val: u64) -> Self {
        Self(val)
    }

    /// Return the raw u64 token value.
    #[inline]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Return the strictly next token.
    #[inline]
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl fmt::Display for FencingToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "fence#{}", self.0)
    }
}

/// Common error types across the HTAP storage engine.
#[derive(Debug, Error)]
pub enum HtapError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Corruption error: {0}")]
    Corruption(String),

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Invalid argument: {0}")]
    InvalidArgument(String),

    #[error("Conflict: {0}")]
    Conflict(String),

    #[error("Fenced: expected token >= {expected}, got {got}")]
    Fenced { expected: u64, got: u64 },

    #[error("Unsupported: {0}")]
    Unsupported(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

/// HTAP common Result type alias.
pub type Result<T> = std::result::Result<T, HtapError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_monotonicity_and_ordering() {
        assert_eq!(Version::INITIAL.get(), 1);
        let v1 = Version::new(1);
        let v2 = v1.next();
        assert_eq!(v2.get(), 2);
        assert!(v2 > v1);
        assert_eq!(v1, Version::INITIAL);
        assert_eq!(v1.to_string(), "v1");
        assert_eq!(v2.to_string(), "v2");
    }

    #[test]
    fn test_fencing_token_comparison_and_ordering() {
        assert_eq!(FencingToken::INITIAL.get(), 1);
        let t1 = FencingToken::new(10);
        let t2 = t1.next();
        assert_eq!(t2.get(), 11);
        assert!(t2 > t1);
        assert!(t1 >= FencingToken::new(10));
        assert!(t1 < FencingToken::new(11));
        assert_eq!(t1.to_string(), "fence#10");
    }

    #[test]
    fn test_error_display_formatting() {
        let err_io = HtapError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "file missing",
        ));
        assert_eq!(err_io.to_string(), "I/O error: file missing");

        let err_corr = HtapError::Corruption("bad checksum".into());
        assert_eq!(err_corr.to_string(), "Corruption error: bad checksum");

        let err_nf = HtapError::NotFound("table users".into());
        assert_eq!(err_nf.to_string(), "Not found: table users");

        let err_arg = HtapError::InvalidArgument("invalid port".into());
        assert_eq!(err_arg.to_string(), "Invalid argument: invalid port");

        let err_conf = HtapError::Conflict("write-write conflict".into());
        assert_eq!(err_conf.to_string(), "Conflict: write-write conflict");

        let err_fence = HtapError::Fenced {
            expected: 5,
            got: 3,
        };
        assert_eq!(err_fence.to_string(), "Fenced: expected token >= 5, got 3");

        let err_unsupp = HtapError::Unsupported("feature X".into());
        assert_eq!(err_unsupp.to_string(), "Unsupported: feature X");

        let err_intern = HtapError::Internal("unexpected panic".into());
        assert_eq!(err_intern.to_string(), "Internal error: unexpected panic");
    }
}
