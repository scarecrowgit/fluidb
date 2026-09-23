//! Length-prefixed JSON protocol used by Unix-domain-socket clients.
//!
//! Each frame consists of a four-byte big-endian payload length followed by one JSON value.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use htap_common::error::HtapError;
use htap_common::Version;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::{BootstrapReport, CatalogSnapshot, Principal};
use htap_sql::result::StatementResult;

/// IPC protocol version required by both clients and owners.
///
/// Bump this version whenever the serde layout of [`IpcRequest`], [`IpcResponse`],
/// [`SessionStatus`], any of their variants, [`sqlparser::ast::Statement`], or any nested
/// payload type carried by those values changes.
pub const IPC_PROTOCOL_VERSION: u32 = 1;

/// Maximum encoded JSON body length accepted by the IPC protocol.
pub const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

/// One operation forwarded to a local server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum IpcRequest {
    /// Verifies protocol compatibility and server-root identity.
    Handshake {
        /// Client protocol version.
        protocol_version: u32,
        /// Canonical server data root expected by the client.
        canonical_root: String,
    },
    /// Executes SQL through the embedded superuser entry point.
    Execute {
        /// SQL statement to execute.
        sql: String,
    },
    /// Executes an already-parsed statement.
    ExecuteBound {
        /// Parsed SQL statement.
        statement: sqlparser::ast::Statement,
    },
    /// Opens a new server session.
    OpenSession,
    /// Authenticates an existing session.
    AuthenticateSession {
        /// Session identifier.
        session_id: u64,
        /// Account name.
        username: String,
        /// Authentication scramble.
        scramble: Vec<u8>,
        /// Client authentication response.
        auth_response: Vec<u8>,
    },
    /// Begins a transaction.
    Begin {
        /// Session identifier.
        session_id: u64,
    },
    /// Checks statement visibility for a session.
    VisibilityCheck {
        /// Session identifier.
        session_id: u64,
        /// Parsed SQL statement.
        statement: sqlparser::ast::Statement,
    },
    /// Loads a catalog snapshot for a session.
    CatalogSnapshot {
        /// Session identifier.
        session_id: u64,
    },
    /// Changes a session's reported maximum packet size.
    SetMaxAllowedPacket {
        /// Session identifier.
        session_id: u64,
        /// Maximum packet size.
        max_allowed_packet: u64,
    },
    /// Reauthenticates and resets an existing session.
    ChangeUser {
        /// Session identifier.
        session_id: u64,
        /// Account name.
        username: String,
        /// Authentication scramble.
        scramble: Vec<u8>,
        /// Client authentication response.
        auth_response: Vec<u8>,
    },
    /// Commits a session transaction.
    Commit {
        /// Session identifier.
        session_id: u64,
    },
    /// Rolls back a session transaction.
    Rollback {
        /// Session identifier.
        session_id: u64,
    },
    /// Resets a session.
    Reset {
        /// Session identifier.
        session_id: u64,
    },
    /// Initializes the root account if account metadata is uninitialized.
    BootstrapRootAccount {
        /// Optional configured root password.
        password: Option<String>,
    },
}

/// Session state returned with every IPC response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStatus {
    /// Whether autocommit is enabled.
    pub autocommit: bool,
    /// Whether the session currently has an open transaction.
    pub in_transaction: bool,
    /// Session identity.
    pub principal: Principal,
}

/// Result returned by an IPC operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IpcResponse {
    /// Session state after processing the operation.
    pub status: SessionStatus,
    /// Operation result or transport-safe server error.
    pub result: std::result::Result<ResponsePayload, WireError>,
}

/// Successful IPC response body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ResponsePayload {
    /// Operation completed without a value.
    Empty,
    /// SQL execution result.
    StatementResult(StatementResult),
    /// Newly allocated session identifier.
    SessionId(u64),
    /// Catalog metadata snapshot.
    CatalogSnapshot(CatalogSnapshot),
    /// Root-account bootstrap result.
    BootstrapReport(BootstrapReport),
}

/// Serializable representation of [`HtapError`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireError {
    /// I/O failure, represented by its stable error-kind name.
    Io {
        /// Debug name of [`std::io::ErrorKind`].
        kind: String,
    },
    /// Corrupt persisted state.
    Corruption(String),
    /// Requested object was not found.
    NotFound(String),
    /// Invalid input.
    InvalidArgument(String),
    /// Retryable conflict.
    Conflict(String),
    /// The IPC owner was unreachable before a request could be completed.
    OwnerUnreachable(String),
    /// Stale fencing token.
    Fenced {
        /// Minimum accepted token.
        expected: u64,
        /// Supplied token.
        got: u64,
    },
    /// Authorization failure.
    PermissionDenied(String),
    /// Monotonic counter overflow.
    CounterOverflow {
        /// Counter name.
        counter: String,
    },
    /// Durable transaction requiring completion.
    DurablePending {
        /// Transaction identifier.
        txn_id: u64,
        /// Assigned commit version.
        version: Version,
        /// Failure reason.
        reason: String,
    },
    /// Transaction manager recovery latch.
    RecoveryRequired {
        /// Transaction blocking new work.
        blocking_txn: u64,
        /// Recovery reason.
        reason: String,
    },
    /// Ambiguous operation outcome.
    Ambiguous(String),
    /// Unsupported operation.
    Unsupported(String),
    /// Internal invariant failure.
    Internal(String),
}

impl From<HtapError> for WireError {
    fn from(error: HtapError) -> Self {
        match error {
            HtapError::Io(error) => Self::Io {
                kind: format!("{:?}", error.kind()),
            },
            HtapError::Corruption(message) => Self::Corruption(message),
            HtapError::NotFound(message) => Self::NotFound(message),
            HtapError::InvalidArgument(message) => Self::InvalidArgument(message),
            HtapError::Conflict(message) => Self::Conflict(message),
            HtapError::Fenced { expected, got } => Self::Fenced { expected, got },
            HtapError::PermissionDenied(message) => Self::PermissionDenied(message),
            HtapError::CounterOverflow { counter } => Self::CounterOverflow {
                counter: counter.to_string(),
            },
            HtapError::DurablePending {
                txn_id,
                version,
                reason,
            } => Self::DurablePending {
                txn_id,
                version,
                reason,
            },
            HtapError::RecoveryRequired {
                blocking_txn,
                reason,
            } => Self::RecoveryRequired {
                blocking_txn,
                reason,
            },
            HtapError::Ambiguous(message) => Self::Ambiguous(message),
            HtapError::Unsupported(message) => Self::Unsupported(message),
            HtapError::Internal(message) => Self::Internal(message),
        }
    }
}

impl From<WireError> for HtapError {
    fn from(error: WireError) -> Self {
        match error {
            WireError::Io { kind } => {
                HtapError::Io(std::io::Error::new(parse_error_kind(&kind), kind))
            }
            WireError::Corruption(message) => Self::Corruption(message),
            WireError::NotFound(message) => Self::NotFound(message),
            WireError::InvalidArgument(message) => Self::InvalidArgument(message),
            WireError::Conflict(message) => Self::Conflict(message),
            WireError::OwnerUnreachable(message) => {
                Self::Conflict(format!("locked but unreachable: {message}"))
            }
            WireError::Fenced { expected, got } => Self::Fenced { expected, got },
            WireError::PermissionDenied(message) => Self::PermissionDenied(message),
            WireError::CounterOverflow { counter } => Self::CounterOverflow {
                counter: known_counter_name(&counter),
            },
            WireError::DurablePending {
                txn_id,
                version,
                reason,
            } => Self::DurablePending {
                txn_id,
                version,
                reason,
            },
            WireError::RecoveryRequired {
                blocking_txn,
                reason,
            } => Self::RecoveryRequired {
                blocking_txn,
                reason,
            },
            WireError::Ambiguous(message) => Self::Ambiguous(message),
            WireError::Unsupported(message) => Self::Unsupported(message),
            WireError::Internal(message) => Self::Internal(message),
        }
    }
}

/// This list must be kept in sync with every `CounterOverflow { counter: name }` site in the workspace.
fn known_counter_name(counter: &str) -> &'static str {
    match counter {
        "account_id" => "account_id",
        "catalog_generation" => "catalog_generation",
        "partition_id" => "partition_id",
        "replica_generation" => "replica_generation",
        "replica_id" => "replica_id",
        "table_id" => "table_id",
        "tablet_id" => "tablet_id",
        "txn_id" => "txn_id",
        "version" => "version",
        "analyze_null_count" => "analyze_null_count",
        "analyze_row_count" => "analyze_row_count",
        "c" => "c",
        "count" => "count",
        "compaction_collapsed_versions" => "compaction_collapsed_versions",
        "compaction_dropped_entries" => "compaction_dropped_entries",
        "compaction_entries_in" => "compaction_entries_in",
        "fencing_token" => "fencing_token",
        "sst_id" => "sst_id",
        _ => "unknown",
    }
}

fn parse_error_kind(kind: &str) -> std::io::ErrorKind {
    use std::io::ErrorKind;

    match kind {
        "NotFound" => ErrorKind::NotFound,
        "PermissionDenied" => ErrorKind::PermissionDenied,
        "ConnectionRefused" => ErrorKind::ConnectionRefused,
        "ConnectionReset" => ErrorKind::ConnectionReset,
        "ConnectionAborted" => ErrorKind::ConnectionAborted,
        "NotConnected" => ErrorKind::NotConnected,
        "AddrInUse" => ErrorKind::AddrInUse,
        "AddrNotAvailable" => ErrorKind::AddrNotAvailable,
        "BrokenPipe" => ErrorKind::BrokenPipe,
        "AlreadyExists" => ErrorKind::AlreadyExists,
        "WouldBlock" => ErrorKind::WouldBlock,
        "InvalidInput" => ErrorKind::InvalidInput,
        "InvalidData" => ErrorKind::InvalidData,
        "TimedOut" => ErrorKind::TimedOut,
        "WriteZero" => ErrorKind::WriteZero,
        "Interrupted" => ErrorKind::Interrupted,
        "UnexpectedEof" => ErrorKind::UnexpectedEof,
        "OutOfMemory" => ErrorKind::OutOfMemory,
        _ => ErrorKind::Other,
    }
}

/// Writes one length-prefixed JSON frame.
///
/// Serialization errors, including excessive nesting, are reported as
/// [`WireError::InvalidArgument`]. The body is fully validated before any bytes are written.
///
/// # Errors
///
/// Returns [`WireError::InvalidArgument`] for invalid JSON values, excessive nesting, or an
/// oversized body, and [`WireError::Io`] for output failures.
pub fn write_frame<W, T>(writer: &mut W, value: &T) -> std::result::Result<(), WireError>
where
    W: Write,
    T: Serialize,
{
    let json = serde_json::to_value(value).map_err(json_error)?;
    if contains_non_finite_float(&json) {
        return Err(WireError::InvalidArgument(
            "invalid IPC JSON payload: non-finite Float64 value".into(),
        ));
    }

    let body = serde_json::to_vec(&json).map_err(json_error)?;
    if body.len() > MAX_FRAME_SIZE {
        return Err(WireError::InvalidArgument(format!(
            "IPC frame size {} exceeds maximum {}",
            body.len(),
            MAX_FRAME_SIZE
        )));
    }

    let length = u32::try_from(body.len()).map_err(|_| {
        WireError::InvalidArgument(format!(
            "IPC frame size {} exceeds the length-prefix capacity",
            body.len()
        ))
    })?;
    writer
        .write_all(&length.to_be_bytes())
        .map_err(WireError::from)?;
    writer.write_all(&body).map_err(WireError::from)
}

fn contains_non_finite_float(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Array(values) => values.iter().any(contains_non_finite_float),
        serde_json::Value::Object(object) => {
            object
                .get("Float64")
                .is_some_and(serde_json::Value::is_null)
                || object.values().any(contains_non_finite_float)
        }
        _ => false,
    }
}

/// Reads one length-prefixed JSON frame.
///
/// The declared body length is checked before allocating the body buffer. An oversized declared
/// length is a framing error: its body remains unread in the stream. JSON decode errors occur only
/// after the complete declared body has been consumed.
///
/// # Errors
///
/// Returns [`WireError::InvalidArgument`] for an oversized, malformed, non-finite, or excessively
/// nested JSON body, and [`WireError::Io`] for input failures.
pub fn read_frame<R, T>(reader: &mut R) -> std::result::Result<T, WireError>
where
    R: Read,
    T: DeserializeOwned,
{
    let mut prefix = [0u8; 4];
    reader.read_exact(&mut prefix).map_err(WireError::from)?;
    read_frame_body(reader, prefix)
}

/// Reads one frame while applying one absolute deadline to every socket read.
///
/// This is used only during handshakes. Established IPC sessions intentionally have no deadline.
pub(crate) fn read_frame_until<T>(
    stream: &mut UnixStream,
    deadline: Instant,
) -> std::result::Result<T, WireError>
where
    T: DeserializeOwned,
{
    let mut prefix = [0u8; 4];
    read_exact_until(stream, &mut prefix, deadline)?;
    read_frame_body_until(stream, prefix, deadline)
}

fn read_frame_body<R, T>(reader: &mut R, prefix: [u8; 4]) -> std::result::Result<T, WireError>
where
    R: Read,
    T: DeserializeOwned,
{
    let length = checked_frame_length(prefix)?;
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).map_err(WireError::from)?;
    serde_json::from_slice(&body).map_err(json_error)
}

fn read_frame_body_until<T>(
    stream: &mut UnixStream,
    prefix: [u8; 4],
    deadline: Instant,
) -> std::result::Result<T, WireError>
where
    T: DeserializeOwned,
{
    let length = checked_frame_length(prefix)?;
    let mut body = vec![0u8; length];
    read_exact_until(stream, &mut body, deadline)?;
    serde_json::from_slice(&body).map_err(json_error)
}

fn checked_frame_length(prefix: [u8; 4]) -> std::result::Result<usize, WireError> {
    let length = u32::from_be_bytes(prefix) as usize;
    if length > MAX_FRAME_SIZE {
        return Err(WireError::InvalidArgument(format!(
            "IPC frame size {length} exceeds maximum {MAX_FRAME_SIZE}"
        )));
    }
    Ok(length)
}

fn read_exact_until(
    stream: &mut UnixStream,
    buffer: &mut [u8],
    deadline: Instant,
) -> std::result::Result<(), WireError> {
    let mut offset = 0;
    while offset < buffer.len() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(WireError::Io {
                kind: "TimedOut".into(),
            });
        }
        stream
            .set_read_timeout(Some(remaining.max(Duration::from_millis(1))))
            .map_err(WireError::from)?;
        match stream.read(&mut buffer[offset..]) {
            Ok(0) => {
                return Err(WireError::Io {
                    kind: "UnexpectedEof".into(),
                });
            }
            Ok(read) => offset += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(WireError::from(error)),
        }
    }
    Ok(())
}

fn json_error(error: serde_json::Error) -> WireError {
    WireError::InvalidArgument(format!("invalid IPC JSON payload: {error}"))
}

impl From<std::io::Error> for WireError {
    fn from(error: std::io::Error) -> Self {
        Self::Io {
            kind: format!("{:?}", error.kind()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read};

    use htap_common::types::{ColumnDef, DataType, Row, Value};
    use htap_sql::result::QueryResult;

    use super::*;

    fn statement(sql: &str) -> sqlparser::ast::Statement {
        htap_sql::parse_one(sql).expect("statement parses")
    }

    fn round_trip<T>(value: &T) -> T
    where
        T: Serialize + DeserializeOwned,
    {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, value).expect("frame writes");
        read_frame(&mut Cursor::new(bytes)).expect("frame reads")
    }

    fn status() -> SessionStatus {
        SessionStatus {
            autocommit: true,
            in_transaction: false,
            principal: Principal::Superuser,
        }
    }

    #[test]
    fn all_requests_round_trip() {
        let requests = vec![
            IpcRequest::Handshake {
                protocol_version: 16,
                canonical_root: "/tmp/fluidb".into(),
            },
            IpcRequest::Execute {
                sql: "SELECT 1".into(),
            },
            IpcRequest::ExecuteBound {
                statement: statement("SELECT 1"),
            },
            IpcRequest::OpenSession,
            IpcRequest::AuthenticateSession {
                session_id: 1,
                username: "root".into(),
                scramble: vec![0, 1, 255],
                auth_response: vec![254, 128, 0],
            },
            IpcRequest::Begin { session_id: 2 },
            IpcRequest::VisibilityCheck {
                session_id: 3,
                statement: statement("SELECT 2"),
            },
            IpcRequest::CatalogSnapshot { session_id: 4 },
            IpcRequest::SetMaxAllowedPacket {
                session_id: 5,
                max_allowed_packet: 1_048_576,
            },
            IpcRequest::ChangeUser {
                session_id: 6,
                username: "alice".into(),
                scramble: vec![3, 2, 1],
                auth_response: vec![9, 8, 7],
            },
            IpcRequest::Commit { session_id: 7 },
            IpcRequest::Rollback { session_id: 8 },
            IpcRequest::Reset { session_id: 9 },
            IpcRequest::BootstrapRootAccount {
                password: Some("secret".into()),
            },
        ];

        for request in requests {
            assert_eq!(round_trip(&request), request);
        }
    }

    #[test]
    fn all_response_payloads_round_trip() {
        let payloads = vec![
            ResponsePayload::Empty,
            ResponsePayload::StatementResult(StatementResult::ddl(1)),
            ResponsePayload::SessionId(42),
            ResponsePayload::CatalogSnapshot(CatalogSnapshot::empty()),
            ResponsePayload::BootstrapReport(BootstrapReport {
                created_root: true,
                config_password_matches_root: Some(true),
            }),
        ];

        for payload in payloads {
            let response = IpcResponse {
                status: status(),
                result: Ok(payload),
            };
            assert_eq!(round_trip(&response), response);
        }
    }

    #[test]
    fn all_errors_are_mirrored() {
        let errors = vec![
            HtapError::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "closed",
            )),
            HtapError::Corruption("corrupt".into()),
            HtapError::NotFound("missing".into()),
            HtapError::InvalidArgument("bad".into()),
            HtapError::Conflict("conflict".into()),
            HtapError::Fenced {
                expected: 9,
                got: 3,
            },
            HtapError::PermissionDenied("denied".into()),
            HtapError::CounterOverflow { counter: "version" },
            HtapError::DurablePending {
                txn_id: 11,
                version: Version::new(12),
                reason: "pending".into(),
            },
            HtapError::RecoveryRequired {
                blocking_txn: 13,
                reason: "recover".into(),
            },
            HtapError::Ambiguous("unknown".into()),
            HtapError::Unsupported("unsupported".into()),
            HtapError::Internal("internal".into()),
        ];

        for error in errors {
            let expected = error.to_string();
            let wire = WireError::from(error);
            let decoded: WireError = round_trip(&wire);
            let restored = HtapError::from(decoded);
            match restored {
                HtapError::Io(ref io_error) => {
                    assert_eq!(io_error.kind(), std::io::ErrorKind::BrokenPipe)
                }
                _ => assert_eq!(restored.to_string(), expected),
            }
        }
    }

    #[test]
    fn counter_overflow_names_use_known_static_values_or_fallback() {
        let known = [
            "account_id",
            "catalog_generation",
            "partition_id",
            "replica_generation",
            "replica_id",
            "table_id",
            "tablet_id",
            "txn_id",
            "version",
            "analyze_null_count",
            "analyze_row_count",
            "c",
            "count",
            "compaction_collapsed_versions",
            "compaction_dropped_entries",
            "compaction_entries_in",
            "fencing_token",
            "sst_id",
        ];

        for name in known {
            let error = HtapError::from(WireError::CounterOverflow {
                counter: name.into(),
            });
            assert!(matches!(
                error,
                HtapError::CounterOverflow { counter } if counter == name
            ));
        }

        let error = HtapError::from(WireError::CounterOverflow {
            counter: "future-counter".into(),
        });
        assert!(matches!(
            error,
            HtapError::CounterOverflow { counter: "unknown" }
        ));
    }

    #[test]
    fn non_utf8_binary_values_round_trip() {
        let result = IpcResponse {
            status: status(),
            result: Ok(ResponsePayload::StatementResult(StatementResult::Query(
                QueryResult::new(
                    vec![ColumnDef {
                        name: "payload".into(),
                        data_type: DataType::Bytes,
                        nullable: false,
                        primary_key: false,
                    }],
                    vec![Row::new(vec![Value::Bytes(vec![
                        0, 0x7f, 0x80, 0xfe, 0xff,
                    ])])],
                ),
            ))),
        };

        assert_eq!(round_trip(&result), result);
    }

    #[test]
    fn non_finite_floats_are_rejected_and_negative_zero_is_preserved() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let result = ResponsePayload::StatementResult(StatementResult::query(
                Vec::new(),
                vec![Row::new(vec![Value::Float64(value)])],
            ));
            let mut bytes = Vec::new();
            assert!(matches!(
                write_frame(&mut bytes, &result),
                Err(WireError::InvalidArgument(_))
            ));
            assert!(bytes.is_empty());
        }

        let result = ResponsePayload::StatementResult(StatementResult::query(
            Vec::new(),
            vec![Row::new(vec![Value::Float64(-0.0)])],
        ));
        let decoded: ResponsePayload = round_trip(&result);
        let ResponsePayload::StatementResult(StatementResult::Query(query)) = decoded else {
            panic!("expected query result");
        };
        let Value::Float64(value) = query.rows[0].values()[0] else {
            panic!("expected float");
        };
        assert_eq!(value.to_bits(), (-0.0f64).to_bits());
    }

    #[test]
    fn handshake_frame_deadline_is_absolute_when_peer_trickles() {
        let root = tempfile::tempdir().expect("temporary socket directory is created");
        let socket_path = root.path().join("handshake.sock");
        let listener =
            std::os::unix::net::UnixListener::bind(&socket_path).expect("listener binds");

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("client connects");
            // A complete frame containing `{}` would take 200 ms to arrive.
            for (index, byte) in [0, 0, 0, 2, b'{', b'}'].into_iter().enumerate() {
                if stream.write_all(&[byte]).is_err() {
                    break;
                }
                if index != 5 {
                    std::thread::sleep(Duration::from_millis(40));
                }
            }
        });

        let mut stream =
            std::os::unix::net::UnixStream::connect(&socket_path).expect("client connects");
        let started = Instant::now();
        let result = read_frame_until::<serde_json::Value>(
            &mut stream,
            started + Duration::from_millis(100),
        );
        let elapsed = started.elapsed();

        // Unix socket read timeouts may be reported as WouldBlock or TimedOut.
        assert!(matches!(
            result,
            Err(WireError::Io { kind })
                if kind == "TimedOut" || kind == "WouldBlock"
        ));
        assert!(
            elapsed < Duration::from_millis(180),
            "read exceeded its absolute deadline: {elapsed:?}"
        );

        server.join().expect("server exits");
    }

    #[test]
    fn deeply_nested_ast_is_rejected_without_panicking() {
        let mut body = String::from(r#"{"array":"#);
        for _ in 0..170 {
            body.push('[');
        }
        body.push_str("null");
        for _ in 0..170 {
            body.push(']');
        }
        body.push('}');

        let mut frame = Vec::with_capacity(4 + body.len());
        frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
        frame.extend_from_slice(body.as_bytes());

        let result = read_frame::<_, serde_json::Value>(&mut Cursor::new(frame));
        assert!(matches!(result, Err(WireError::InvalidArgument(_))));
    }

    struct PrefixOnlyReader {
        prefix: Cursor<[u8; 4]>,
        body_was_read: bool,
    }

    impl Read for PrefixOnlyReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if self.prefix.position() < 4 {
                return self.prefix.read(buffer);
            }
            self.body_was_read = true;
            panic!("oversized frame body must not be read");
        }
    }

    #[test]
    fn oversized_request_and_response_frames_are_rejected_before_allocation() {
        for response in [false, true] {
            let declared = (MAX_FRAME_SIZE as u32 + 1).to_be_bytes();
            let mut reader = PrefixOnlyReader {
                prefix: Cursor::new(declared),
                body_was_read: false,
            };
            if response {
                let result = read_frame::<_, IpcResponse>(&mut reader);
                assert!(matches!(result, Err(WireError::InvalidArgument(_))));
            } else {
                let result = read_frame::<_, IpcRequest>(&mut reader);
                assert!(matches!(result, Err(WireError::InvalidArgument(_))));
            }
            assert!(!reader.body_was_read);
        }
    }

    #[test]
    fn oversized_write_is_rejected_without_writing() {
        let request = IpcRequest::Execute {
            sql: "x".repeat(MAX_FRAME_SIZE + 1),
        };
        let mut output = Vec::new();
        assert!(matches!(
            write_frame(&mut output, &request),
            Err(WireError::InvalidArgument(_))
        ));
        assert!(output.is_empty());
    }
}
