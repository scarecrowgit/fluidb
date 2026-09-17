//! Network client speaking the MySQL text protocol to an `htapd` / [`htap_wire::WireServer`].

use std::net::ToSocketAddrs;

use htap_common::{HtapError, Result, Version};
use htap_sql::StatementResult;
use htap_wire::{ClientOptions, WireClient, WireResult};

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
/// statements sent over this same connection until `COMMIT`. There is still no TLS; query text
/// and results travel in cleartext unless the connection is otherwise tunneled.
#[derive(Debug)]
pub struct RemoteClient {
    client: WireClient,
}

impl RemoteClient {
    /// Connects and authenticates with the default user name and the given password.
    ///
    /// # Errors
    ///
    /// Returns [`HtapError::Io`] if the connection fails and [`HtapError::Internal`] if
    /// the server rejects the credentials (MySQL error 1045) or the handshake.
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
    /// Server errors are mapped by code: 1064 → [`HtapError::InvalidArgument`],
    /// 1146 → [`HtapError::NotFound`], 1213 → [`HtapError::Conflict`],
    /// 1235 → [`HtapError::Unsupported`], anything else → [`HtapError::Internal`].
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
