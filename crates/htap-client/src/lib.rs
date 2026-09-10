//! Embedded client for the HTAP database engine.
//!
//! # Architecture & Direct LocalServer Façade
//!
//! [`EmbeddedClient`] is a synchronous, in-process, direct façade over [`htap_server::LocalServer`].
//! It executes SQL statements directly against local storage and transaction engines within the
//! calling process, with no intermediate RPC, serialization, or network hop.
//!
//! # Supported SQL Subset
//!
//! The client supports the synchronous single-partition SQL subset implemented by the engine:
//! - `CREATE TABLE`: Schema definitions specifying typed columns and primary keys.
//! - Literal `INSERT`: Single- or multi-row inserts with literal value lists.
//! - Complete-PK `DELETE`: Point deletes matching the complete primary key in the `WHERE` clause.
//! - Complete-PK `SELECT`: Point lookups projecting expressions or all columns matching the complete primary key in the `WHERE` clause.
//!
//! # Explicit Scope Limitations & Non-Features
//!
//! This embedded client explicitly does **not** provide:
//! - **No network transport**: No host, port, socket binding, or remote connection handling.
//! - **No MySQL wire protocol**: No wire protocol framing, handshake negotiation, or MySQL client/driver compatibility.
//! - **No session state**: Each statement executes independently without connection-level state, session variables, or multi-statement transaction handles.
//! - **No prepared statements**: Queries are parsed and planned synchronously on each call without prepared statement handles or binary parameter binding.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::path::PathBuf;

use htap_common::Result;
use htap_server::LocalServer;

pub use htap_sql::{CommandResult, QueryResult, StatementResult};

/// Synchronous in-process embedded database client.
///
/// Direct façade over [`LocalServer`] executing synchronous operations within
/// the current process memory.
///
/// # Supported SQL Operations
/// - `CREATE TABLE`
/// - Literal `INSERT`
/// - Complete-PK `DELETE`
/// - Complete-PK `SELECT`
///
/// # Unsupported Features & Limitations
/// Does **not** support network connections (no host or port), MySQL wire protocol
/// or client driver compatibility, sessions, or prepared statements.
pub struct EmbeddedClient {
    server: LocalServer,
}

impl EmbeddedClient {
    /// Opens or recovers an embedded database instance rooted at `root`.
    ///
    /// Initializes or recovers catalog snapshots, LSM rowstore storage,
    /// transaction logging journals, and local mover state under the specified path.
    ///
    /// # Errors
    ///
    /// Returns [`htap_common::HtapError`] if directory creation, catalog recovery,
    /// or storage initialization fails.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let server = LocalServer::open(root)?;
        Ok(Self { server })
    }

    /// Synchronously executes a single SQL statement against the embedded server.
    ///
    /// # Supported Subset
    /// - `CREATE TABLE`
    /// - Literal `INSERT`
    /// - Complete-PK `DELETE`
    /// - Complete-PK `SELECT`
    ///
    /// # Errors
    ///
    /// Returns [`htap_common::HtapError`] on SQL syntax error, catalog conflict,
    /// nonexistent table, schema or type mismatch, missing primary key predicate,
    /// or underlying storage failure.
    pub fn execute(&self, sql: &str) -> Result<StatementResult> {
        self.server.execute(sql)
    }
}
