//! MySQL text-protocol network layer for the HTAP engine.
//!
//! [`server::WireServer`] exposes a [`htap_server::LocalServer`] over TCP to any MySQL
//! client (`mysql` CLI, drivers, [`client::WireClient`]). The protocol implementation is
//! hand-written and synchronous: one thread per connection, each owning one
//! [`htap_server::Session`] for its lifetime (Phase 10), so `BEGIN`/`COMMIT`/`ROLLBACK`,
//! autocommit, and session variables behave the same as calling that `Session` directly.
//!
//! Supported: handshake v10 with `mysql_native_password`, `COM_QUERY` (text result sets)
//! including `CLIENT_MULTI_STATEMENTS` batches when negotiated, prepared statements (binary
//! protocol), `COM_PING`, `COM_INIT_DB`, `COM_QUIT`, `COM_RESET_CONNECTION`, `COM_CHANGE_USER`,
//! explicit transactions and session variables via [`htap_server::Session`], multi-packet
//! messages beyond 16MB, and a small start-up compatibility shim ([`shim`]) for statements the
//! engine itself cannot answer. Not supported: TLS, compression, server-side cursors
//! (`COM_STMT_FETCH`).
//!
//! See [`server`] for the security contract.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod binary_codec;
pub mod client;
pub mod codec;
pub mod compression;
pub mod error_map;
pub mod handshake;
pub mod prepared;
pub mod proto;
pub mod result_codec;
pub mod server;
pub mod sha1;
pub mod shim;
pub mod tls;

pub use client::{
    ClientOptions, ClientPreparedStatement, CompressionMode, MultiQueryOutcome, TlsMode,
    WireClient, WireResult,
};
pub use error_map::{map_htap_error, wire_error_to_htap, WireError};
pub use result_codec::OkPacket;
pub use server::{TlsConfig, WireServer, WireServerConfig};
