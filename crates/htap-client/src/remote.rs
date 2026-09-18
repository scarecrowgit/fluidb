//! Network client speaking the MySQL text protocol to an `htapd` / [`htap_wire::WireServer`].

use std::net::ToSocketAddrs;

use htap_common::types::{ColumnDef, Value};
use htap_common::{HtapError, Result, Version};
use htap_sql::StatementResult;
use htap_wire::{ClientOptions, ClientPreparedStatement, WireClient, WireResult};

/// A prepared statement handle returned by [`RemoteClient::prepare`] (Phase 11 plan task 5).
///
/// Deliberately holds only the statement id and metadata, not a borrow of (or reference to) the
/// [`RemoteClient`] that created it: unlike a typical prepared-statement API that ties the
/// handle's lifetime to its connection, a `PreparedStatement` here is a plain, independent value.
/// This is a borrow-checker consequence of Rust, not a protocol one — a handle borrowing
/// `&mut RemoteClient` would make it impossible to use the same client for anything else (another
/// prepared statement, a plain `execute`, ...) while the handle is alive. Instead,
/// [`RemoteClient::execute_prepared`] and [`RemoteClient::close_prepared`] take the handle
/// alongside `&mut self`; reusing a statement across multiple executes means keeping both the
/// `RemoteClient` and the `PreparedStatement` alive together (e.g. as two local variables), and
/// nothing prevents mismatching a `PreparedStatement` with a *different* `RemoteClient` than the
/// one that prepared it — doing so surfaces as an ordinary `ER_UNKNOWN_STMT_HANDLER` server error
/// (`HtapError::Internal`; see [`RemoteClient::execute_prepared`]'s errors) rather than a
/// compile-time guarantee.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedStatement {
    stmt_id: u32,
    /// Number of `?` placeholders in the prepared statement text.
    pub num_params: u16,
    /// Best-effort result-set column schema: empty when the statement produces no result set, or
    /// its shape could not be statically inferred (see
    /// `htap_sql::resolve_prepare_output_schema`'s doc for exactly when).
    pub columns: Vec<ColumnDef>,
}

/// Synchronous network client for a remote fluidb server.
///
/// Results are decoded back into the same [`StatementResult`] shape that
/// [`crate::EmbeddedClient`] returns, so code can switch between in-process and networked
/// execution without changing how it reads results. Server-side errors are mapped back to
/// stable [`HtapError`] categories by their MySQL error code.
///
/// One `RemoteClient` connection is one server-side `htap_server::Session` for its whole
/// lifetime (Phase 10): sending `BEGIN`/`COMMIT`/`ROLLBACK` or `SET autocommit = 0` as ordinary
/// SQL through [`RemoteClient::execute`] opens and manages an explicit transaction exactly as
/// it would for an embedded session, with the transaction's buffered writes visible only to
/// statements sent over this same connection until `COMMIT`. TLS is available through
/// [`ClientOptions::tls`], and protocol compression is available through
/// [`ClientOptions::compression`].
#[derive(Debug)]
pub struct RemoteClient {
    client: WireClient,
}

impl RemoteClient {
    /// Connects and authenticates with the default user name and the given password.
    ///
    /// # Errors
    ///
    /// Returns [`HtapError::Io`] if the connection fails, [`HtapError::PermissionDenied`]
    /// if the server rejects the credentials (MySQL error 1045), and possibly
    /// [`HtapError::Internal`] for handshake issues.
    pub fn connect<A: ToSocketAddrs>(addr: A, password: Option<&str>) -> Result<Self> {
        let client = WireClient::connect(addr, password)?;
        Ok(Self { client })
    }

    /// Connects with explicit [`ClientOptions`].
    pub fn connect_with<A: ToSocketAddrs>(addr: A, options: ClientOptions) -> Result<Self> {
        let client = WireClient::connect_with(addr, options)?;
        Ok(Self { client })
    }

    /// Executes a single SQL statement on the server.
    ///
    /// # Errors
    ///
    /// Server errors are mapped by code: 1045/1142 → [`HtapError::PermissionDenied`],
    /// 1064 → [`HtapError::InvalidArgument`], 1146 → [`HtapError::NotFound`],
    /// 1213 → [`HtapError::Conflict`], 1235 → [`HtapError::Unsupported`],
    /// anything else → [`HtapError::Internal`].
    pub fn execute(&mut self, sql: &str) -> Result<StatementResult> {
        match self.client.query(sql)? {
            WireResult::Ok(ok) => Ok(decode_command(ok.affected_rows, &ok.info)?),
            WireResult::Rows { columns, rows } => Ok(StatementResult::query(columns, rows)),
        }
    }

    /// Sends `COM_PING`.
    pub fn ping(&mut self) -> Result<()> {
        self.client.ping()?;
        Ok(())
    }

    /// `COM_STMT_PREPARE` (Phase 11 plan task 5): prepares `sql` on the server and returns a
    /// handle that can be executed (repeatedly, with different parameters) via
    /// [`Self::execute_prepared`] and eventually discarded via [`Self::close_prepared`]. See
    /// [`PreparedStatement`]'s doc comment for why the handle does not borrow `self`.
    ///
    /// # Errors
    ///
    /// Server errors are mapped the same way as [`Self::execute`] (e.g. an unsupported statement
    /// shape, or a placeholder in an unsupported position, maps to
    /// [`HtapError::Unsupported`]/[`HtapError::InvalidArgument`]).
    pub fn prepare(&mut self, sql: &str) -> Result<PreparedStatement> {
        let p = self.client.prepare(sql)?;
        Ok(PreparedStatement {
            stmt_id: p.stmt_id,
            num_params: p.num_params,
            columns: p.columns,
        })
    }

    /// `COM_STMT_EXECUTE` (Phase 11 plan task 5): binds `params`, in order, to `stmt`'s `?`
    /// placeholders and executes it, exactly like [`Self::execute`] would for the equivalent
    /// literal SQL.
    ///
    /// # Errors
    ///
    /// Returns [`HtapError::Internal`] (the server's `ER_UNKNOWN_STMT_HANDLER` has no dedicated
    /// [`HtapError`] mapping) if `stmt` is unknown to the server this connection is talking to
    /// (e.g. already closed, or prepared on a different connection); otherwise the same error
    /// mapping as [`Self::execute`].
    pub fn execute_prepared(
        &mut self,
        stmt: &PreparedStatement,
        params: &[Value],
    ) -> Result<StatementResult> {
        let inner = ClientPreparedStatement {
            stmt_id: stmt.stmt_id,
            num_params: stmt.num_params,
            columns: stmt.columns.clone(),
        };
        match self.client.execute_prepared(&inner, params)? {
            WireResult::Ok(ok) => decode_command(ok.affected_rows, &ok.info),
            WireResult::Rows { columns, rows } => Ok(StatementResult::query(columns, rows)),
        }
    }

    /// `COM_STMT_CLOSE` (Phase 11 plan task 5); consumes the handle, since it is no longer valid
    /// to execute afterward. No response by protocol, so this only fails on a transport error.
    pub fn close_prepared(&mut self, stmt: PreparedStatement) -> Result<()> {
        self.client.close_stmt(stmt.stmt_id)?;
        Ok(())
    }

    /// Server version string reported in the handshake.
    pub fn server_version(&self) -> &str {
        self.client.server_version()
    }

    /// Sends `COM_QUIT` and closes the connection.
    pub fn close(self) -> Result<()> {
        self.client.quit()?;
        Ok(())
    }
}

/// Decodes the private `info` convention of `htap-wire`: empty for DDL, `version=<n>` or
/// `version=none` for DML.
fn decode_command(affected: u64, info: &str) -> Result<StatementResult> {
    if info.is_empty() {
        return Ok(StatementResult::ddl(affected));
    }
    let Some(v) = info.strip_prefix("version=") else {
        return Ok(StatementResult::ddl(affected));
    };
    if v == "none" {
        return Ok(StatementResult::dml(affected, None));
    }
    let n: u64 = v
        .parse()
        .map_err(|_| HtapError::Internal(format!("malformed version info '{info}'")))?;
    Ok(StatementResult::dml(affected, Some(Version::new(n))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_command_convention() {
        assert_eq!(decode_command(1, "").unwrap(), StatementResult::ddl(1));
        assert_eq!(
            decode_command(2, "version=5").unwrap(),
            StatementResult::dml(2, Some(Version::new(5)))
        );
        assert_eq!(
            decode_command(0, "version=none").unwrap(),
            StatementResult::dml(0, None)
        );
        assert!(decode_command(0, "version=x").is_err());
    }
}
