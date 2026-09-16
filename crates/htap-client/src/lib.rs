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
//! The client supports the synchronous SQL subset implemented by the engine across
//! unpartitioned and partitioned tables:
//! - `CREATE TABLE`: Schema definitions specifying typed columns and primary keys. Supports unpartitioned tables (default single partition `"p0"`) as well as partitioned tables via MySQL `PARTITION BY RANGE [COLUMNS]` and `PARTITION BY LIST [COLUMNS]` (including final `VALUES LESS THAN MAXVALUE`). Partition options, subpartitioning, expressions, multi-column COLUMNS, and non-final MAXVALUE are strictly rejected.
//! - `ALTER TABLE`: Typed partition lifecycle operations: `ADD PARTITION`, `DROP PARTITION`, and `REORGANIZE PARTITION` for strict finite range and list forms and final `MAXVALUE` where supported. Enforces empty-partition safety gates: dropping or reorganizing populated source partitions is strictly rejected with `HtapError::InvalidArgument` to prevent data loss. Other ALTER statements, partition options, subpartitioning, and hash partitioning are rejected.
//! - Literal `INSERT`: Single- or multi-row inserts with literal value lists. On partitioned tables, rows are routed by partition key and committed atomically in a single transaction payload and version.
//! - Complete-PK `DELETE`: Point deletes matching the complete primary key in the `WHERE` clause, routed to the target partition.
//! - Complete-PK `SELECT`: Point lookups projecting expressions or all columns matching the complete primary key in the `WHERE` clause, routed to the target partition while strictly preserving the rowstore fast path.
//! - Analytic `SELECT`: Narrow OLAP scans projecting columns or aggregates (`COUNT`, `SUM`, `MIN`, `MAX`) with AND-only filters and optional `GROUP BY`, scanning all partitions of the table at a single visible snapshot and combining results.
//!
//! # Network access
//!
//! [`RemoteClient`] connects to an `htapd` daemon (or an embedded [`htap_wire::WireServer`])
//! over TCP using the MySQL text protocol and returns the same [`StatementResult`] shape.
//!
//! # Explicit Scope Limitations & Non-Features
//!
//! This embedded client explicitly does **not** provide:
//! - **No network transport in `EmbeddedClient`**: it executes in-process; use [`RemoteClient`] for TCP.
//! - **No session state**: Each statement executes independently without connection-level state, session variables, or multi-statement transaction handles.
//! - **No prepared statements**: Queries are parsed and planned synchronously on each call without prepared statement handles or binary parameter binding.
//! - **Deferred partition & storage capabilities**: Physical data migration for populated partition reorganization, physical storage reclamation for dropped partitions, delete vectors, compaction, autonomous background conversion scheduling, hash/multiple tablets, distributed/remote movement, consensus/HA, and full MySQL compatibility remain deferred.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::path::PathBuf;

use htap_common::Result;
use htap_server::LocalServer;

pub mod remote;

pub use htap_sql::{CommandResult, QueryResult, StatementResult};
pub use htap_wire::ClientOptions;
pub use remote::RemoteClient;

/// Synchronous in-process embedded database client.
///
/// Direct façade over [`LocalServer`] executing synchronous operations within
/// the current process memory.
///
/// # Supported SQL Operations
/// - `CREATE TABLE` (unpartitioned or partitioned via MySQL `PARTITION BY RANGE/LIST`)
/// - `ALTER TABLE` (typed partition lifecycle: `ADD PARTITION`, `DROP PARTITION`, `REORGANIZE PARTITION` on empty sources)
/// - Literal `INSERT` (routes by partition key on partitioned tables, atomic multi-row commit)
/// - Complete-PK `DELETE` (routes by partition key)
/// - Complete-PK `SELECT` (routes by partition key, preserving rowstore fast path)
/// - Analytic `SELECT` (scans all partitions at current visible snapshot)
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
    /// - `CREATE TABLE` (unpartitioned or partitioned via MySQL `PARTITION BY RANGE/LIST`)
    /// - `ALTER TABLE` (typed partition lifecycle: `ADD PARTITION`, `DROP PARTITION`, `REORGANIZE PARTITION` on empty sources)
    /// - Literal `INSERT` (routes by partition key on partitioned tables, single transaction version)
    /// - Complete-PK `DELETE` (routed by partition key)
    /// - Complete-PK `SELECT` (routed by partition key, takes rowstore fast path)
    /// - Analytic `SELECT` (scans all partitions at current visible snapshot)
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
