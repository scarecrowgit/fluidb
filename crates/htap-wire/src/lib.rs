//! MySQL text-protocol network layer for the HTAP engine.
//!
//! [`server::WireServer`] exposes a [`htap_server::LocalServer`] over TCP to any MySQL
//! client (`mysql` CLI, drivers, [`client::WireClient`]). The protocol implementation is
//! hand-written and synchronous: one thread per connection, each owning one
//! [`htap_server::Session`] for its lifetime (Phase 10), so `BEGIN`/`COMMIT`/`ROLLBACK`,
//! autocommit, and session variables behave the same as calling that `Session` directly.
//!
//! Supported: handshake v10 with `mysql_native_password`, `COM_QUERY` (text result sets),
//! `COM_PING`, `COM_INIT_DB`, `COM_QUIT`, explicit transactions and session variables via
//! [`htap_server::Session`], and a small start-up compatibility shim ([`shim`]) for statements
//! the engine itself cannot answer. Not supported: TLS, compression, prepared statements /
//! binary protocol, multi-statements, multi-results, and payloads of 16MB or more.
//!
//! See [`server`] for the security contract.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod client;
pub mod codec;
pub mod error_map;
pub mod handshake;
pub mod proto;
pub mod result_codec;
pub mod server;
pub mod sha1;
pub mod shim;

pub use client::{ClientOptions, WireClient, WireResult};
pub use error_map::{map_htap_error, wire_error_to_htap, WireError};
pub use result_codec::OkPacket;
pub use server::{WireServer, WireServerConfig};
