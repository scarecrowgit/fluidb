//! MySQL client/server protocol constants used by the wire layer.
//!
//! Only the subset needed for the text protocol (handshake, `COM_QUERY`, result sets)
//! is defined here. Values follow the public MySQL protocol documentation.

/// Server version string advertised in the initial handshake.
///
/// References `htap_sql::variables::REPORTED_VERSION` directly (rather than duplicating the
/// literal) so this and `@@version`/`SELECT VERSION()` can never drift apart; see
/// `version_constant_matches_variable_registry` below.
pub const SERVER_VERSION: &str = htap_sql::variables::REPORTED_VERSION;

/// Protocol version byte of the initial handshake packet.
pub const PROTOCOL_VERSION: u8 = 10;

/// Authentication plugin implemented by the server.
pub const AUTH_PLUGIN_NATIVE: &str = "mysql_native_password";

/// Logical database name accepted by `COM_INIT_DB` / `USE`.
pub const DEFAULT_SCHEMA: &str = "htap";

/// Database names accepted by `COM_INIT_DB` / `USE`.
pub const ACCEPTED_SCHEMAS: [&str; 3] = ["htap", "fluidb", "default"];

// Capability flags.
/// `CLIENT_LONG_PASSWORD`.
pub const CLIENT_LONG_PASSWORD: u32 = 0x0000_0001;
/// `CLIENT_FOUND_ROWS`.
pub const CLIENT_FOUND_ROWS: u32 = 0x0000_0002;
/// `CLIENT_LONG_FLAG`.
pub const CLIENT_LONG_FLAG: u32 = 0x0000_0004;
/// `CLIENT_CONNECT_WITH_DB`.
pub const CLIENT_CONNECT_WITH_DB: u32 = 0x0000_0008;
/// `CLIENT_PROTOCOL_41`.
pub const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
/// `CLIENT_SSL`.
pub const CLIENT_SSL: u32 = 0x0000_0800;
/// `CLIENT_TRANSACTIONS`.
pub const CLIENT_TRANSACTIONS: u32 = 0x0000_2000;
/// `CLIENT_SECURE_CONNECTION`.
pub const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;
/// `CLIENT_PLUGIN_AUTH`.
pub const CLIENT_PLUGIN_AUTH: u32 = 0x0008_0000;
/// `CLIENT_CONNECT_ATTRS`.
pub const CLIENT_CONNECT_ATTRS: u32 = 0x0010_0000;
/// `CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA`.
pub const CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA: u32 = 0x0020_0000;
/// `CLIENT_DEPRECATE_EOF`.
pub const CLIENT_DEPRECATE_EOF: u32 = 0x0100_0000;
/// `CLIENT_MULTI_STATEMENTS` (Phase 11 plan task 10): advertised by the server and honored by
/// [`crate::server::respond_query`] only for a connection whose handshake response negotiated it
/// (see `Session::multi_statements`); a `COM_QUERY` containing more than one statement is still
/// rejected exactly as before for a connection that never negotiates this flag.
pub const CLIENT_MULTI_STATEMENTS: u32 = 0x0001_0000;
/// `CLIENT_MULTI_RESULTS` (Phase 11 plan task 10): advertised alongside `CLIENT_MULTI_STATEMENTS`
/// (real clients expect both; MySQL itself always sets `CLIENT_MULTI_RESULTS` together with
/// `CLIENT_MULTI_STATEMENTS`). This server has no stored procedures, so the only source of
/// multiple result sets is a multi-statement `COM_QUERY` batch, already gated on
/// `CLIENT_MULTI_STATEMENTS`; nothing separately checks this bit.
pub const CLIENT_MULTI_RESULTS: u32 = 0x0002_0000;

/// Capabilities advertised by the server. TLS (`CLIENT_SSL`), compression, and session tracking
/// are deliberately absent.
pub const SERVER_CAPABILITIES: u32 = CLIENT_LONG_PASSWORD
    | CLIENT_FOUND_ROWS
    | CLIENT_LONG_FLAG
    | CLIENT_CONNECT_WITH_DB
    | CLIENT_PROTOCOL_41
    | CLIENT_TRANSACTIONS
    | CLIENT_SECURE_CONNECTION
    | CLIENT_PLUGIN_AUTH
    | CLIENT_CONNECT_ATTRS
    | CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA
    | CLIENT_DEPRECATE_EOF
    | CLIENT_MULTI_STATEMENTS
    | CLIENT_MULTI_RESULTS;

/// `SERVER_STATUS_AUTOCOMMIT`: set whenever the session's `autocommit` is on (Phase 11 fix pass,
/// finding 8: previously hardcoded unconditionally, ignoring `SET autocommit = 0`).
pub const SERVER_STATUS_AUTOCOMMIT: u16 = 0x0002;
/// `SERVER_STATUS_IN_TRANS`: set whenever a transaction — explicit (`BEGIN`) or implicit
/// (autocommit off) — is open (Phase 11 fix pass, finding 8: previously never set at all).
pub const SERVER_STATUS_IN_TRANS: u16 = 0x0001;
/// `SERVER_MORE_RESULTS_EXISTS` (Phase 11 plan task 10): set on every result (resultset
/// terminator or command OK) but the last one in a `CLIENT_MULTI_STATEMENTS` batch; see
/// `result_codec::build_command_ok`/`build_resultset_terminator`.
pub const SERVER_MORE_RESULTS_EXISTS: u16 = 0x0008;

/// Character set / collation id for `utf8mb4_general_ci`.
pub const COLLATION_UTF8MB4: u16 = 45;
/// Character set / collation id for `binary`.
pub const COLLATION_BINARY: u16 = 63;

// Command bytes.
/// `COM_QUIT`.
pub const COM_QUIT: u8 = 0x01;
/// `COM_INIT_DB`.
pub const COM_INIT_DB: u8 = 0x02;
/// `COM_QUERY`.
pub const COM_QUERY: u8 = 0x03;
/// `COM_PING`.
pub const COM_PING: u8 = 0x0e;
/// `COM_CHANGE_USER` (Phase 11 plan task 7: dispatched by [`crate::server::respond_change_user`]).
pub const COM_CHANGE_USER: u8 = 0x11;
/// `COM_STMT_PREPARE` (Phase 11 plan task 4: dispatched by [`crate::server::respond_stmt_prepare`]).
pub const COM_STMT_PREPARE: u8 = 0x16;
/// `COM_STMT_EXECUTE` (Phase 11 plan task 4: dispatched by [`crate::server::respond_stmt_execute`]).
pub const COM_STMT_EXECUTE: u8 = 0x17;
/// `COM_STMT_SEND_LONG_DATA` (Phase 11 plan task 4: dispatched by [`crate::server`]'s command
/// loop, which appends to the target statement's buffered parameter via
/// [`crate::prepared::PreparedStatementRegistry::append_long_data`]).
pub const COM_STMT_SEND_LONG_DATA: u8 = 0x18;
/// `COM_STMT_CLOSE` (Phase 11 plan task 4: dispatched by [`crate::server`]'s command loop, which
/// discards the statement via [`crate::prepared::PreparedStatementRegistry::close`]).
pub const COM_STMT_CLOSE: u8 = 0x19;
/// `COM_STMT_RESET` (Phase 11 plan task 4: dispatched by [`crate::server::respond_stmt_reset`]).
pub const COM_STMT_RESET: u8 = 0x1a;
/// `COM_STMT_FETCH` (rejected: server-side cursors are not implemented).
pub const COM_STMT_FETCH: u8 = 0x1c;
/// `COM_RESET_CONNECTION` (Phase 11 plan task 6: dispatched by
/// [`crate::server::respond_reset_connection`]).
pub const COM_RESET_CONNECTION: u8 = 0x1f;

// Column types.
/// `MYSQL_TYPE_DECIMAL`.
pub const MYSQL_TYPE_DECIMAL: u8 = 0x00;
/// `MYSQL_TYPE_TINY`.
pub const MYSQL_TYPE_TINY: u8 = 0x01;
/// `MYSQL_TYPE_SHORT`.
pub const MYSQL_TYPE_SHORT: u8 = 0x02;
/// `MYSQL_TYPE_LONG`.
pub const MYSQL_TYPE_LONG: u8 = 0x03;
/// `MYSQL_TYPE_FLOAT`.
pub const MYSQL_TYPE_FLOAT: u8 = 0x04;
/// `MYSQL_TYPE_DOUBLE`.
pub const MYSQL_TYPE_DOUBLE: u8 = 0x05;
/// `MYSQL_TYPE_NULL`.
pub const MYSQL_TYPE_NULL: u8 = 0x06;
/// `MYSQL_TYPE_TIMESTAMP`.
pub const MYSQL_TYPE_TIMESTAMP: u8 = 0x07;
/// `MYSQL_TYPE_LONGLONG`.
pub const MYSQL_TYPE_LONGLONG: u8 = 0x08;
/// `MYSQL_TYPE_INT24`.
pub const MYSQL_TYPE_INT24: u8 = 0x09;
/// `MYSQL_TYPE_DATE`.
pub const MYSQL_TYPE_DATE: u8 = 0x0a;
/// `MYSQL_TYPE_TIME` (rejected: no engine representation).
pub const MYSQL_TYPE_TIME: u8 = 0x0b;
/// `MYSQL_TYPE_DATETIME`.
pub const MYSQL_TYPE_DATETIME: u8 = 0x0c;
/// `MYSQL_TYPE_YEAR`.
pub const MYSQL_TYPE_YEAR: u8 = 0x0d;
/// `MYSQL_TYPE_VARCHAR`.
pub const MYSQL_TYPE_VARCHAR: u8 = 0x0f;
/// `MYSQL_TYPE_NEWDECIMAL`.
pub const MYSQL_TYPE_NEWDECIMAL: u8 = 0xf6;
/// `MYSQL_TYPE_ENUM`.
pub const MYSQL_TYPE_ENUM: u8 = 0xf7;
/// `MYSQL_TYPE_SET`.
pub const MYSQL_TYPE_SET: u8 = 0xf8;
/// `MYSQL_TYPE_TINY_BLOB`.
pub const MYSQL_TYPE_TINY_BLOB: u8 = 0xf9;
/// `MYSQL_TYPE_MEDIUM_BLOB`.
pub const MYSQL_TYPE_MEDIUM_BLOB: u8 = 0xfa;
/// `MYSQL_TYPE_LONG_BLOB`.
pub const MYSQL_TYPE_LONG_BLOB: u8 = 0xfb;
/// `MYSQL_TYPE_BLOB`.
pub const MYSQL_TYPE_BLOB: u8 = 0xfc;
/// `MYSQL_TYPE_VAR_STRING`.
pub const MYSQL_TYPE_VAR_STRING: u8 = 0xfd;
/// `MYSQL_TYPE_STRING`.
pub const MYSQL_TYPE_STRING: u8 = 0xfe;

/// `CURSOR_TYPE_NO_CURSOR`, the only `COM_STMT_EXECUTE` flags value this server accepts (no
/// server-side cursor support).
pub const CURSOR_TYPE_NO_CURSOR: u8 = 0x00;

// Column flags.
/// `NOT_NULL_FLAG`.
pub const NOT_NULL_FLAG: u16 = 0x0001;
/// `PRI_KEY_FLAG`.
pub const PRI_KEY_FLAG: u16 = 0x0002;
/// `UNSIGNED_FLAG`.
pub const UNSIGNED_FLAG: u16 = 0x0020;
/// `BINARY_FLAG`.
pub const BINARY_FLAG: u16 = 0x0080;

// Packet header bytes.
/// OK packet header.
pub const OK_HEADER: u8 = 0x00;
/// EOF packet header (also the header of a `CLIENT_DEPRECATE_EOF` result-set terminator).
pub const EOF_HEADER: u8 = 0xfe;
/// ERR packet header.
pub const ERR_HEADER: u8 = 0xff;
/// NULL marker in a text result row.
pub const NULL_MARKER: u8 = 0xfb;

// Wire error codes (with SQLSTATE) used by the server for protocol-level conditions.
/// `ER_ACCESS_DENIED_ERROR`.
pub const ER_ACCESS_DENIED: (u16, &str) = (1045, "28000");
/// `ER_CON_COUNT_ERROR`.
pub const ER_TOO_MANY_CONNECTIONS: (u16, &str) = (1040, "08004");
/// `ER_UNKNOWN_COM_ERROR`.
pub const ER_UNKNOWN_COMMAND: (u16, &str) = (1047, "08S01");
/// `ER_BAD_DB_ERROR`.
pub const ER_BAD_DB: (u16, &str) = (1049, "42000");
/// `ER_NOT_SUPPORTED_AUTH_MODE`.
pub const ER_NOT_SUPPORTED_AUTH_MODE: (u16, &str) = (1251, "08004");
/// `ER_UNKNOWN_ERROR`.
pub const ER_UNKNOWN: (u16, &str) = (1105, "HY000");
/// `ER_NET_PACKET_TOO_LARGE` (Phase 11 plan task 8: sent by [`crate::server`]'s command loop and
/// [`crate::codec::read_message_with_stop`]'s callers when a message's declared length would
/// exceed `max_allowed_packet`).
pub const ER_NET_PACKET_TOO_LARGE: (u16, &str) = (1153, "08S01");
/// `ER_UNKNOWN_STMT_HANDLER`: an `EXECUTE`/`CLOSE`/`RESET`/`SEND_LONG_DATA` referencing a
/// statement id this connection never prepared or already closed (Phase 11 plan task 3/4).
pub const ER_UNKNOWN_STMT_HANDLER: (u16, &str) = (1243, "HY000");

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards against the two reported version strings drifting apart again: `SERVER_VERSION`
    /// references `htap_sql::variables::REPORTED_VERSION` directly, so this is trivially true
    /// today, but it fails loudly if a future edit replaces the reference with a hardcoded
    /// literal (Phase 10 plan, task 9).
    #[test]
    fn version_constant_matches_variable_registry() {
        assert_eq!(SERVER_VERSION, htap_sql::variables::REPORTED_VERSION);
    }
}
