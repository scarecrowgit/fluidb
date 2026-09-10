//! MVCC version domain and leadership fencing tokens.

use serde::{Deserialize, Serialize};
use std::fmt;

use crate::error::{HtapError, Result};

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

    /// Return the strictly next version, or [`HtapError::CounterOverflow`] on overflow.
    #[inline]
    pub fn checked_next(self) -> Result<Self> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(HtapError::CounterOverflow { counter: "version" })
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

    /// Return the strictly next token, or [`HtapError::CounterOverflow`] on overflow.
    #[inline]
    pub fn checked_next(self) -> Result<Self> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(HtapError::CounterOverflow {
                counter: "fencing_token",
            })
    }
}

impl fmt::Display for FencingToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "fence#{}", self.0)
    }
}

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

        let v_checked = v1.checked_next().unwrap();
        assert_eq!(v_checked, v2);

        let v_max = Version::new(u64::MAX);
        let err = v_max.checked_next().unwrap_err();
        assert!(matches!(
            err,
            HtapError::CounterOverflow { counter: "version" }
        ));
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

        let t_checked = t1.checked_next().unwrap();
        assert_eq!(t_checked, t2);

        let t_max = FencingToken::new(u64::MAX);
        let err = t_max.checked_next().unwrap_err();
        assert!(matches!(
            err,
            HtapError::CounterOverflow {
                counter: "fencing_token"
            }
        ));
    }
}
