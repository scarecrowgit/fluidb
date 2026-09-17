//! Error packets and the mapping between [`HtapError`] and MySQL error codes.

use std::fmt;
use std::io;

use htap_common::HtapError;

use crate::codec::{read_fixed, read_u16};
use crate::proto::*;

/// Maps an engine error to a MySQL `(error_code, SQLSTATE)` pair.
pub fn map_htap_error(err: &HtapError) -> (u16, &'static str) {
    match err {
        HtapError::InvalidArgument(_) => (1064, "42000"),
        HtapError::NotFound(_) => (1146, "42S02"),
        HtapError::Conflict(_) => (1213, "40001"),
        HtapError::Unsupported(_) => (1235, "42000"),
        HtapError::Io(_)
        | HtapError::Corruption(_)
        | HtapError::Fenced { .. }
        | HtapError::CounterOverflow { .. }
        | HtapError::DurablePending { .. }
        | HtapError::RecoveryRequired { .. }
        | HtapError::Internal(_) => ER_UNKNOWN,
    }
}

/// Maps a MySQL error code received over the wire back to an [`HtapError`].
///
/// Protocol-level codes with no engine counterpart (access denied, too many connections,
/// unknown command, bad database) become [`HtapError::Internal`] carrying the message.
pub fn wire_error_to_htap(code: u16, message: String) -> HtapError {
    match code {
        1064 => HtapError::InvalidArgument(message),
        1146 => HtapError::NotFound(message),
        1213 => HtapError::Conflict(message),
        1235 => HtapError::Unsupported(message),
        _ => HtapError::Internal(format!("server error {code}: {message}")),
    }
}

/// Builds an ERR packet payload.
pub fn build_err_payload(code: u16, sqlstate: &str, message: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(9 + message.len());
    buf.push(ERR_HEADER);
    buf.extend_from_slice(&code.to_le_bytes());
    buf.push(b'#');
    let mut state = sqlstate.as_bytes().to_vec();
    state.resize(5, b'0');
    buf.extend_from_slice(&state[..5]);
    buf.extend_from_slice(message.as_bytes());
    buf
}

/// Parses an ERR packet payload into `(code, sqlstate, message)`.
pub fn parse_err_payload(payload: &[u8]) -> io::Result<(u16, String, String)> {
    let mut pos = 0;
    let header = read_fixed(payload, &mut pos, 1)?[0];
    if header != ERR_HEADER {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not an ERR packet",
        ));
    }
    let code = read_u16(payload, &mut pos)?;
    let mut sqlstate = String::new();
    if payload.get(pos) == Some(&b'#') {
        pos += 1;
        sqlstate = String::from_utf8_lossy(read_fixed(payload, &mut pos, 5)?).into_owned();
    }
    let message = String::from_utf8_lossy(&payload[pos..]).into_owned();
    Ok((code, sqlstate, message))
}

/// Errors surfaced by the client side of the wire protocol.
#[derive(Debug)]
pub enum WireError {
    /// Transport failure.
    Io(io::Error),
    /// The server answered with an ERR packet.
    Server {
        /// MySQL error code.
        code: u16,
        /// SQLSTATE.
        sqlstate: String,
        /// Human-readable message.
        message: String,
    },
    /// The peer violated the protocol.
    Protocol(String),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Server {
                code,
                sqlstate,
                message,
            } => write!(f, "server error {code} ({sqlstate}): {message}"),
            Self::Protocol(m) => write!(f, "protocol error: {m}"),
        }
    }
}

impl std::error::Error for WireError {}

impl From<io::Error> for WireError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<WireError> for HtapError {
    fn from(e: WireError) -> Self {
        match e {
            WireError::Io(io) => HtapError::Io(io),
            WireError::Server { code, message, .. } => wire_error_to_htap(code, message),
            WireError::Protocol(m) => HtapError::Internal(format!("wire protocol error: {m}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use htap_common::Version;

    #[test]
    fn error_map_all_htap_variants() {
        let cases: Vec<(HtapError, u16)> = vec![
            (HtapError::InvalidArgument("x".into()), 1064),
            (HtapError::NotFound("x".into()), 1146),
            (HtapError::Conflict("x".into()), 1213),
            (HtapError::Unsupported("x".into()), 1235),
            (HtapError::Io(io::Error::other("x")), 1105),
            (HtapError::Corruption("x".into()), 1105),
            (
                HtapError::Fenced {
                    expected: 1,
                    got: 0,
                },
                1105,
            ),
            (HtapError::CounterOverflow { counter: "c" }, 1105),
            (
                HtapError::DurablePending {
                    txn_id: 1,
                    version: Version::new(1),
                    reason: "x".into(),
                },
                1105,
            ),
            (
                HtapError::RecoveryRequired {
                    blocking_txn: 1,
                    reason: "x".into(),
                },
                1105,
            ),
            (HtapError::Internal("x".into()), 1105),
        ];
        for (err, code) in cases {
            assert_eq!(map_htap_error(&err).0, code, "{err}");
        }
        assert!(matches!(
            wire_error_to_htap(1064, "m".into()),
            HtapError::InvalidArgument(_)
        ));
        assert!(matches!(
            wire_error_to_htap(1146, "m".into()),
            HtapError::NotFound(_)
        ));
        assert!(matches!(
            wire_error_to_htap(1213, "m".into()),
            HtapError::Conflict(_)
        ));
        assert!(matches!(
            wire_error_to_htap(1235, "m".into()),
            HtapError::Unsupported(_)
        ));
        assert!(matches!(
            wire_error_to_htap(1045, "m".into()),
            HtapError::Internal(_)
        ));
    }

    #[test]
    fn durable_pending_never_maps_to_the_retryable_conflict_code() {
        // Storage-reviewer finding F1: a session that surfaces `DurablePending` after an
        // ambiguous commit must never be reported to a MySQL client as 1213/`40001`
        // ("rolled back, retry"), since the write may already have applied; a client that
        // retries on that code could double-apply it.
        let err = HtapError::DurablePending {
            txn_id: 1,
            version: Version::new(2),
            reason: "commit sync failed at decision boundary".into(),
        };
        let mapped = map_htap_error(&err);
        assert_ne!(mapped.0, 1213, "DurablePending must not map to 1213");
        assert_ne!(mapped.1, "40001", "DurablePending must not map to 40001");
        assert_eq!(mapped, ER_UNKNOWN, "DurablePending maps to 1105/HY000");
    }

    #[test]
    fn recovery_required_never_maps_to_the_retryable_conflict_code() {
        // Fix-pass item 5: a session commit rejected because the manager is latched behind an
        // unrelated, still-ambiguous transaction must never be reported as 1213/`40001`
        // ("rolled back, retry") either — the caller's own transaction never committed at all,
        // but it is not a write-write conflict, and the wording matters for client retry logic.
        let err = HtapError::RecoveryRequired {
            blocking_txn: 9,
            reason: "manager is latched pending recovery of an earlier ambiguous commit".into(),
        };
        let mapped = map_htap_error(&err);
        assert_ne!(mapped.0, 1213, "RecoveryRequired must not map to 1213");
        assert_ne!(mapped.1, "40001", "RecoveryRequired must not map to 40001");
        assert_eq!(mapped, ER_UNKNOWN, "RecoveryRequired maps to 1105/HY000");
    }

    #[test]
    fn err_packet_round_trip() {
        let payload = build_err_payload(1146, "42S02", "table 't' not found");
        assert_eq!(payload[0], ERR_HEADER);
        let (code, state, msg) = parse_err_payload(&payload).unwrap();
        assert_eq!(code, 1146);
        assert_eq!(state, "42S02");
        assert_eq!(msg, "table 't' not found");
        assert!(parse_err_payload(&[0x00]).is_err());
    }
}
