//! Error type shared by every crate in the engine.

use thiserror::Error;

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
