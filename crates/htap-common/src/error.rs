//! Error type shared by every crate in the engine.

use thiserror::Error;

use crate::version::Version;

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

    #[error("Permission denied: {0}")]
    PermissionDenied(String),

    #[error("Counter overflow: {counter}")]
    CounterOverflow { counter: &'static str },

    /// Transaction commit was fsynced and is durable, but post-commit
    /// execution (apply or publish) failed. Transaction cannot be rolled back;
    /// recovery/completion is required.
    #[error("durable commit pending completion for txn {txn_id} at version {version}: {reason}; recovery required")]
    DurablePending {
        txn_id: u64,
        version: Version,
        reason: String,
    },

    /// A transaction manager is latched pending recovery of an earlier, still-ambiguous commit
    /// (`blocking_txn`): this transaction itself was rejected before any work was done and
    /// definitely did not commit, so it is safe to retry once recovery resolves the blocking
    /// transaction. Distinct from [`Self::DurablePending`] (whose own commit outcome is
    /// ambiguous) and from [`Self::Conflict`] (retryable "rolled back"): a client must not treat
    /// this the same as a rolled-back-and-retryable write, since the *caller's* transaction is
    /// simply queued behind someone else's unresolved one, not aborted for a write conflict.
    #[error("manager is latched pending recovery of txn {blocking_txn}: {reason}")]
    RecoveryRequired { blocking_txn: u64, reason: String },

    #[error("Ambiguous outcome: {0}")]
    Ambiguous(String),

    #[error("Unsupported: {0}")]
    Unsupported(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

impl HtapError {
    /// Returns true if this error is [`HtapError::DurablePending`].
    pub fn is_durable_pending(&self) -> bool {
        matches!(self, Self::DurablePending { .. })
    }

    /// Returns true if this error is [`HtapError::RecoveryRequired`].
    pub fn is_recovery_required(&self) -> bool {
        matches!(self, Self::RecoveryRequired { .. })
    }

    /// Returns true if this error is [`HtapError::Ambiguous`].
    pub fn is_ambiguous(&self) -> bool {
        matches!(self, Self::Ambiguous(_))
    }

    /// Returns true if this error is [`HtapError::PermissionDenied`].
    pub fn is_permission_denied(&self) -> bool {
        matches!(self, Self::PermissionDenied(_))
    }
}

/// HTAP common Result type alias.
pub type Result<T> = std::result::Result<T, HtapError>;

#[cfg(test)]
mod tests {
    use super::*;

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

        let err_overflow = HtapError::CounterOverflow { counter: "version" };
        assert_eq!(err_overflow.to_string(), "Counter overflow: version");

        let err_dp = HtapError::DurablePending {
            txn_id: 42,
            version: Version::new(5),
            reason: "apply failed on participant 1".into(),
        };
        assert_eq!(
            err_dp.to_string(),
            "durable commit pending completion for txn 42 at version v5: apply failed on participant 1; recovery required"
        );
        assert!(err_dp.is_durable_pending());
        assert!(!err_fence.is_durable_pending());
        assert!(!err_dp.is_recovery_required());

        let err_rr = HtapError::RecoveryRequired {
            blocking_txn: 7,
            reason: "commit sync failed at decision boundary".into(),
        };
        assert_eq!(
            err_rr.to_string(),
            "manager is latched pending recovery of txn 7: commit sync failed at decision boundary"
        );
        assert!(err_rr.is_recovery_required());
        assert!(!err_rr.is_durable_pending());

        let err_ambiguous = HtapError::Ambiguous("commit outcome unknown".into());
        assert_eq!(
            err_ambiguous.to_string(),
            "Ambiguous outcome: commit outcome unknown"
        );
        assert!(err_ambiguous.is_ambiguous());
        assert!(!err_rr.is_ambiguous());

        let err_unsupp = HtapError::Unsupported("feature X".into());
        assert_eq!(err_unsupp.to_string(), "Unsupported: feature X");

        let err_intern = HtapError::Internal("unexpected panic".into());
        assert_eq!(err_intern.to_string(), "Internal error: unexpected panic");
    }
}
