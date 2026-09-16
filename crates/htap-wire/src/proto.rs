//! MySQL client/server protocol constants used by the wire layer.
//!
//! Only the subset needed for the text protocol (handshake, `COM_QUERY`, result sets)
//! is defined here. Values follow the public MySQL protocol documentation.

/// Server version string advertised in the initial handshake.
pub const SERVER_VERSION: &str = "8.0.0-fluidb-0.1.0";

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

/// Capabilities advertised by the server. TLS (`CLIENT_SSL`), compression, multi-statements,
/// multi-results and session tracking are deliberately absent.
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
    | CLIENT_DEPRECATE_EOF;

/// `SERVER_STATUS_AUTOCOMMIT`: every statement auto-commits; `SERVER_STATUS_IN_TRANS` is never set.
pub const SERVER_STATUS_AUTOCOMMIT: u16 = 0x0002;

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
/// `COM_CHANGE_USER` (rejected).
pub const COM_CHANGE_USER: u8 = 0x11;
/// `COM_STMT_PREPARE` (rejected: binary protocol is not implemented).
pub const COM_STMT_PREPARE: u8 = 0x16;
/// `COM_STMT_EXECUTE` (rejected: binary protocol is not implemented).
pub const COM_STMT_EXECUTE: u8 = 0x17;
/// `COM_RESET_CONNECTION` (rejected).
pub const COM_RESET_CONNECTION: u8 = 0x1f;

// Column types.
/// `MYSQL_TYPE_TINY`.
pub const MYSQL_TYPE_TINY: u8 = 0x01;
/// `MYSQL_TYPE_LONG`.
pub const MYSQL_TYPE_LONG: u8 = 0x03;
/// `MYSQL_TYPE_DOUBLE`.
pub const MYSQL_TYPE_DOUBLE: u8 = 0x05;
/// `MYSQL_TYPE_LONGLONG`.
pub const MYSQL_TYPE_LONGLONG: u8 = 0x08;
/// `MYSQL_TYPE_DATETIME`.
pub const MYSQL_TYPE_DATETIME: u8 = 0x0c;
/// `MYSQL_TYPE_BLOB`.
pub const MYSQL_TYPE_BLOB: u8 = 0xfc;
/// `MYSQL_TYPE_VAR_STRING`.
pub const MYSQL_TYPE_VAR_STRING: u8 = 0xfd;

// Column flags.
/// `NOT_NULL_FLAG`.
pub const NOT_NULL_FLAG: u16 = 0x0001;
/// `PRI_KEY_FLAG`.
pub const PRI_KEY_FLAG: u16 = 0x0002;
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
