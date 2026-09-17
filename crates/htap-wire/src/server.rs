//! TCP listener speaking the MySQL text protocol on top of [`LocalServer`].
//!
//! # Model
//!
//! One accept thread and one thread per connection. Every connection shares a single
//! `Arc<LocalServer>`; statements are serialized by the server's own execution lock, so
//! the wire layer holds no global lock of its own. Since Phase 10, each authenticated
//! connection owns one [`htap_server::Session`] for its whole lifetime: `BEGIN`/`COMMIT`/
//! `ROLLBACK`, autocommit, and session variables all behave exactly as they do for
//! [`htap_server::Session::execute`] directly. A connection that disconnects mid-transaction
//! (`QUIT`, EOF, a framing error, or server shutdown) has its open transaction rolled back
//! explicitly before the connection thread exits, with the session's own `Drop` impl as a
//! safety net for any path that doesn't.
//!
//! # Security contract
//!
//! - Default bind address is `127.0.0.1:3307` (loopback only). Binding elsewhere is an
//!   explicit opt-in via [`WireServerConfig::listen`].
//! - One implicit user: the user name sent by the client is logged but never checked.
//! - [`WireServerConfig::password`] is the only credential. `None` accepts any client.
//!   `Some(pw)` verifies a `mysql_native_password` response; clients proposing another
//!   plugin are switched to `mysql_native_password`.
//! - No TLS. The password exchange is a challenge/response hash, but query text and result
//!   rows travel in cleartext. Do not bind a non-loopback address without a trusted network
//!   or an external tunnel.
//!
//! # Shutdown
//!
//! [`WireServer::shutdown`] sets a stop flag, stops accepting, and joins every connection
//! thread. Connection reads use [`WireServerConfig::read_timeout`] so idle connections
//! observe the flag at packet boundaries. A connection that is blocked mid-packet keeps
//! waiting for the rest of that packet; the packet is never torn.

use std::io::{self, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use htap_common::HtapError;
use htap_server::LocalServer;
use htap_sql::{CommandResult, StatementResult};
use parking_lot::Mutex;

use crate::codec::{
    read_packet_with_stop, write_lenenc_int, write_packet, SeqCounter, SHUTDOWN_ERROR_KIND,
};
use crate::error_map::{build_err_payload, map_htap_error};
use crate::handshake::{
    generate_scramble, AuthSwitchRequest, HandshakeResponse41, HandshakeV10, SCRAMBLE_LEN,
};
use crate::proto::*;
use crate::result_codec::{
    build_column_def41, build_command_ok, build_resultset_terminator, encode_text_row,
};
use crate::sha1::verify_native_password;
use crate::shim::{is_accepted_schema, try_shim, ShimOutcome};

/// Listener configuration.
#[derive(Debug, Clone)]
pub struct WireServerConfig {
    /// Address to bind. Defaults to `127.0.0.1:3307`.
    pub listen: SocketAddr,
    /// Maximum number of simultaneously open connections; excess connections receive
    /// ERR 1040 and are closed.
    pub max_connections: usize,
    /// Shared password. `None` disables authentication.
    pub password: Option<String>,
    /// Read timeout used to observe shutdown between packets.
    pub read_timeout: Duration,
}

impl Default for WireServerConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 3307)),
            max_connections: 64,
            password: None,
            read_timeout: Duration::from_millis(200),
        }
    }
}

/// Running listener. Dropping it without calling [`WireServer::shutdown`] leaves the
/// threads running until the process exits.
pub struct WireServer {
    local_addr: SocketAddr,
    stop: Arc<AtomicBool>,
    accept_thread: Option<JoinHandle<()>>,
    conn_threads: Arc<Mutex<Vec<JoinHandle<()>>>>,
    connection_count: Arc<AtomicUsize>,
}

impl std::fmt::Debug for WireServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WireServer")
            .field("local_addr", &self.local_addr)
            .field(
                "connections",
                &self.connection_count.load(Ordering::Relaxed),
            )
            .finish()
    }
}

struct Shared {
    server: Arc<LocalServer>,
    password: Option<String>,
    stop: Arc<AtomicBool>,
    read_timeout: Duration,
    next_connection_id: AtomicU32,
}

impl WireServer {
    /// Binds the listener and starts accepting connections.
    pub fn start(config: WireServerConfig, server: Arc<LocalServer>) -> io::Result<Self> {
        let listener = TcpListener::bind(config.listen)?;
        listener.set_nonblocking(true)?;
        let local_addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let conn_threads: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
        let connection_count = Arc::new(AtomicUsize::new(0));
        let shared = Arc::new(Shared {
            server,
            password: config.password.clone(),
            stop: Arc::clone(&stop),
            read_timeout: config.read_timeout,
            next_connection_id: AtomicU32::new(1),
        });
        let max_connections = config.max_connections.max(1);

        let accept_stop = Arc::clone(&stop);
        let accept_threads = Arc::clone(&conn_threads);
        let accept_count = Arc::clone(&connection_count);
        let accept_thread = std::thread::Builder::new()
            .name("htap-wire-accept".into())
            .spawn(move || {
                accept_loop(
                    listener,
                    shared,
                    max_connections,
                    accept_stop,
                    accept_threads,
                    accept_count,
                )
            })?;

        tracing::info!(%local_addr, "htap-wire listening");
        Ok(Self {
            local_addr,
            stop,
            accept_thread: Some(accept_thread),
            conn_threads,
            connection_count,
        })
    }

    /// Address the listener is bound to (useful with port 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Number of currently open connections.
    pub fn connection_count(&self) -> usize {
        self.connection_count.load(Ordering::SeqCst)
    }

    /// Stops accepting, waits for every connection thread, and frees the port.
    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.accept_thread.take() {
            let _ = h.join();
        }
        let handles: Vec<JoinHandle<()>> = std::mem::take(&mut *self.conn_threads.lock());
        for h in handles {
            let _ = h.join();
        }
    }
}

fn accept_loop(
    listener: TcpListener,
    shared: Arc<Shared>,
    max_connections: usize,
    stop: Arc<AtomicBool>,
    conn_threads: Arc<Mutex<Vec<JoinHandle<()>>>>,
    connection_count: Arc<AtomicUsize>,
) {
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, peer)) => {
                conn_threads.lock().retain(|h| !h.is_finished());
                let _ = stream.set_nodelay(true);
                if connection_count.load(Ordering::SeqCst) >= max_connections {
                    tracing::warn!(%peer, "rejecting connection: too many connections");
                    let mut s = stream;
                    let (code, state) = ER_TOO_MANY_CONNECTIONS;
                    let _ = write_packet(
                        &mut s,
                        0,
                        &build_err_payload(code, state, "Too many connections"),
                    );
                    continue;
                }
                connection_count.fetch_add(1, Ordering::SeqCst);
                let shared = Arc::clone(&shared);
                let count = Arc::clone(&connection_count);
                let connection_id = shared.next_connection_id.fetch_add(1, Ordering::Relaxed);
                let spawn = std::thread::Builder::new()
                    .name(format!("htap-wire-conn-{connection_id}"))
                    .spawn(move || {
                        let result = handle_connection(stream, connection_id, &shared);
                        match result {
                            Ok(()) => tracing::debug!(connection_id, "connection closed"),
                            Err(e) => {
                                tracing::debug!(connection_id, error = %e, "connection ended")
                            }
                        }
                        count.fetch_sub(1, Ordering::SeqCst);
                    });
                match spawn {
                    Ok(handle) => conn_threads.lock().push(handle),
                    Err(e) => {
                        connection_count.fetch_sub(1, Ordering::SeqCst);
                        tracing::error!(error = %e, "failed to spawn connection thread");
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                tracing::warn!(error = %e, "accept failed");
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Outcome of the authentication phase.
struct Session {
    deprecate_eof: bool,
}

fn send(stream: &mut TcpStream, seq: &mut SeqCounter, payload: &[u8]) -> io::Result<()> {
    write_packet(stream, seq.advance(), payload)
}

fn send_err(
    stream: &mut TcpStream,
    seq: &mut SeqCounter,
    (code, state): (u16, &str),
    message: &str,
) -> io::Result<()> {
    send(stream, seq, &build_err_payload(code, state, message))
}

fn read(stream: &mut TcpStream, stop: &AtomicBool) -> io::Result<(u8, Vec<u8>)> {
    read_packet_with_stop(stream, Some(stop))
}

fn handle_connection(mut stream: TcpStream, connection_id: u32, shared: &Shared) -> io::Result<()> {
    stream.set_read_timeout(Some(shared.read_timeout))?;
    let session = match authenticate(&mut stream, connection_id, shared)? {
        Some(s) => s,
        None => return Ok(()),
    };
    tracing::debug!(
        connection_id,
        deprecate_eof = session.deprecate_eof,
        "authenticated"
    );

    // One `htap_server::Session` per authenticated connection (Phase 10 task 9): buffered
    // writes, autocommit state, and user/system variables all live here for the connection's
    // whole lifetime.
    let mut server_session = shared.server.open_session();
    let result = run_commands(&mut stream, shared, &session, &mut server_session);
    // Explicit rollback on every path out of `run_commands` (`QUIT`, EOF, a framing error, or
    // server shutdown observed mid-read), regardless of which one was taken; `Session::drop`
    // (invoked when `server_session` goes out of scope right after this) is a safety net for
    // any path that doesn't reach here, e.g. a panic unwinding through this frame.
    let _ = server_session.rollback();
    result
}

/// The connection's command loop, run after authentication with one `htap_server::Session`
/// owned by the caller for the connection's whole lifetime.
fn run_commands(
    stream: &mut TcpStream,
    shared: &Shared,
    session: &Session,
    server_session: &mut htap_server::Session,
) -> io::Result<()> {
    loop {
        let (pkt_seq, payload) = match read(stream, &shared.stop) {
            Ok(p) => p,
            Err(e) if e.kind() == SHUTDOWN_ERROR_KIND => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            // Framing errors (including >= 16MB payloads) desynchronize the stream: close.
            Err(e) => return Err(e),
        };
        let mut seq = SeqCounter::new();
        seq.continue_after(pkt_seq);
        let Some((&command, body)) = payload.split_first() else {
            send_err(stream, &mut seq, ER_UNKNOWN_COMMAND, "Empty command packet")?;
            continue;
        };
        match command {
            COM_QUIT => return Ok(()),
            COM_PING => send(stream, &mut seq, &build_command_ok(0, ""))?,
            COM_INIT_DB => {
                let name = String::from_utf8_lossy(body).into_owned();
                respond_use_db(stream, &mut seq, &name)?;
            }
            COM_QUERY => {
                let sql = match std::str::from_utf8(body) {
                    Ok(s) => s,
                    Err(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "COM_QUERY payload is not valid UTF-8",
                        ))
                    }
                };
                respond_query(stream, &mut seq, sql, session, server_session)?;
            }
            // `COM_STMT_PREPARE` / `COM_STMT_EXECUTE` (binary protocol),
            // `COM_RESET_CONNECTION`, `COM_CHANGE_USER` and every other command are
            // deliberately unsupported and answered with ERR 1047.
            _ => {
                send_err(
                    stream,
                    &mut seq,
                    ER_UNKNOWN_COMMAND,
                    &format!("Unknown command 0x{command:02x}"),
                )?;
            }
        }
    }
}

fn authenticate(
    stream: &mut TcpStream,
    connection_id: u32,
    shared: &Shared,
) -> io::Result<Option<Session>> {
    let mut seq = SeqCounter::new();
    let mut scramble = generate_scramble();
    send(
        stream,
        &mut seq,
        &HandshakeV10::new(connection_id, scramble).encode(),
    )?;

    let (pkt_seq, payload) = match read(stream, &shared.stop) {
        Ok(p) => p,
        Err(e) if e.kind() == SHUTDOWN_ERROR_KIND => return Ok(None),
        Err(e) => return Err(e),
    };
    seq.continue_after(pkt_seq);

    match HandshakeResponse41::peek_capabilities(&payload) {
        Some(caps) if caps & CLIENT_SSL != 0 => {
            send_err(
                stream,
                &mut seq,
                ER_UNKNOWN,
                "TLS is not supported by this server",
            )?;
            return Ok(None);
        }
        Some(_) => {}
        None => {
            send_err(stream, &mut seq, ER_UNKNOWN, "Malformed handshake response")?;
            return Ok(None);
        }
    }
    let response = match HandshakeResponse41::decode(&payload) {
        Ok(r) => r,
        Err(e) => {
            send_err(
                stream,
                &mut seq,
                ER_UNKNOWN,
                &format!("Malformed handshake response: {e}"),
            )?;
            return Ok(None);
        }
    };
    if response.capability_flags & CLIENT_SECURE_CONNECTION == 0 {
        send_err(
            stream,
            &mut seq,
            ER_NOT_SUPPORTED_AUTH_MODE,
            "Client does not support secure authentication",
        )?;
        return Ok(None);
    }

    let mut auth_response = response.auth_response.clone();
    let needs_switch = match response.auth_plugin.as_deref() {
        Some(p) => !p.is_empty() && p != AUTH_PLUGIN_NATIVE,
        None => false,
    };
    if needs_switch {
        scramble = generate_scramble();
        let switch = AuthSwitchRequest {
            plugin: AUTH_PLUGIN_NATIVE.into(),
            scramble,
        };
        send(stream, &mut seq, &switch.encode())?;
        let (pkt_seq, switch_payload) = match read(stream, &shared.stop) {
            Ok(p) => p,
            Err(e) if e.kind() == SHUTDOWN_ERROR_KIND => return Ok(None),
            Err(e) => return Err(e),
        };
        seq.continue_after(pkt_seq);
        auth_response = switch_payload;
    }

    let authorized = match &shared.password {
        None => true,
        Some(pw) => verify_native_password(&scramble_array(&scramble), pw, &auth_response),
    };
    if !authorized {
        tracing::warn!(connection_id, user = %response.username, "access denied");
        send_err(
            stream,
            &mut seq,
            ER_ACCESS_DENIED,
            &format!(
                "Access denied for user '{}' (using password: {})",
                response.username,
                if auth_response.is_empty() {
                    "NO"
                } else {
                    "YES"
                }
            ),
        )?;
        return Ok(None);
    }

    if let Some(db) = response.database.as_deref() {
        if !db.is_empty() && !is_accepted_schema(db) {
            send_err(
                stream,
                &mut seq,
                ER_BAD_DB,
                &format!("Unknown database '{db}'"),
            )?;
            return Ok(None);
        }
    }

    send(stream, &mut seq, &build_command_ok(0, ""))?;
    tracing::debug!(connection_id, user = %response.username, "connection authenticated");
    Ok(Some(Session {
        deprecate_eof: response.capability_flags & CLIENT_DEPRECATE_EOF != 0
            && SERVER_CAPABILITIES & CLIENT_DEPRECATE_EOF != 0,
    }))
}

fn scramble_array(s: &[u8; SCRAMBLE_LEN]) -> [u8; SCRAMBLE_LEN] {
    *s
}

fn respond_use_db(stream: &mut TcpStream, seq: &mut SeqCounter, name: &str) -> io::Result<()> {
    if is_accepted_schema(name) {
        send(stream, seq, &build_command_ok(0, ""))
    } else {
        send_err(
            stream,
            seq,
            ER_BAD_DB,
            &format!("Unknown database '{name}'"),
        )
    }
}

/// Formats the OK-packet `info` string that lets [`crate::client`] recover the exact
/// [`CommandResult`] variant: empty for DDL, `version=<n>` (or `version=none`) for DML.
/// This is a private convention of `htap-wire`, not part of the MySQL protocol.
pub fn command_info(result: &CommandResult) -> String {
    match result {
        CommandResult::Ddl { .. } => String::new(),
        CommandResult::Dml { version, .. } => match version {
            Some(v) => format!("version={}", v.get()),
            None => "version=none".to_string(),
        },
    }
}

/// Encodes a complete response to a query as packets appended to `out`.
pub fn encode_statement_result(
    out: &mut Vec<u8>,
    seq: &mut SeqCounter,
    result: &StatementResult,
    deprecate_eof: bool,
) -> io::Result<()> {
    match result {
        StatementResult::Command(cmd) => {
            let info = command_info(cmd);
            write_packet(out, seq.advance(), &build_command_ok(cmd.affected(), &info))
        }
        StatementResult::Query(qr) => {
            let mut count = Vec::with_capacity(9);
            write_lenenc_int(&mut count, qr.columns.len() as u64);
            write_packet(out, seq.advance(), &count)?;
            for col in &qr.columns {
                write_packet(out, seq.advance(), &build_column_def41(col))?;
            }
            if !deprecate_eof {
                write_packet(out, seq.advance(), &build_resultset_terminator(false))?;
            }
            for row in &qr.rows {
                write_packet(out, seq.advance(), &encode_text_row(row))?;
            }
            write_packet(
                out,
                seq.advance(),
                &build_resultset_terminator(deprecate_eof),
            )
        }
    }
}

fn respond_query(
    stream: &mut TcpStream,
    seq: &mut SeqCounter,
    sql: &str,
    session: &Session,
    server_session: &mut htap_server::Session,
) -> io::Result<()> {
    let outcome: Result<StatementResult, HtapError> = match try_shim(sql) {
        Some(ShimOutcome::Ok) => Ok(StatementResult::ddl(0)),
        Some(ShimOutcome::Rows { columns, rows }) => Ok(StatementResult::query(columns, rows)),
        Some(ShimOutcome::UseDb(name)) => return respond_use_db(stream, seq, &name),
        None => server_session.execute(sql),
    };
    let mut out = Vec::new();
    match outcome {
        Ok(result) => encode_statement_result(&mut out, seq, &result, session.deprecate_eof)?,
        Err(err) => {
            let (code, state) = map_htap_error(&err);
            tracing::debug!(code, error = %err, "statement failed");
            write_packet(
                &mut out,
                seq.advance(),
                &build_err_payload(code, state, &err.to_string()),
            )?;
        }
    }
    stream.write_all(&out)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_are_loopback_only() {
        let cfg = WireServerConfig::default();
        assert_eq!(cfg.listen, SocketAddr::from(([127, 0, 0, 1], 3307)));
        assert!(cfg.listen.ip().is_loopback());
        assert_eq!(cfg.max_connections, 64);
        assert!(cfg.password.is_none());
    }

    #[test]
    fn command_info_convention() {
        use htap_common::Version;
        assert_eq!(command_info(&CommandResult::ddl(1)), "");
        assert_eq!(
            command_info(&CommandResult::dml(2, Some(Version::new(7)))),
            "version=7"
        );
        assert_eq!(command_info(&CommandResult::dml(2, None)), "version=none");
    }
}
