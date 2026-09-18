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
//! - Authentication uses catalog accounts. [`WireServerConfig::password`] bootstraps the root
//!   account on first startup; later password changes are managed with `ALTER USER`.
//! - TLS is optional. When [`WireServerConfig::tls`] is configured, clients can negotiate TLS;
//!   [`WireServerConfig::require_secure_transport`] rejects non-TLS connections. Without TLS,
//!   the password exchange is a challenge/response hash, but query text and result rows travel
//!   in cleartext. Do not bind a non-loopback address without TLS, a trusted network, or an
//!   external tunnel.
//! - Every pre-authentication read (the handshake response, and either side of an auth-plugin
//!   switch) is bounded by [`AUTH_PHASE_MAX_PACKET`] before anything is allocated (finding 1 of
//!   the Phase 11 fix pass): an unauthenticated peer cannot make this server allocate more than
//!   that from a single connection attempt merely by declaring a large packet length and never
//!   sending it.
//!
//! # Shutdown
//!
//! [`WireServer::shutdown`] (Phase 11 plan task 11), in order: (1) sets a stop flag; (2) joins
//! the accept thread (already non-blocking and polling the flag, so this returns quickly and,
//! critically, guarantees no connection can be accepted or registered afterward — see below);
//! (3) force-closes every currently registered connection with `shutdown(Shutdown::Both)` on a
//! `try_clone`d handle to its socket; (4) joins every connection thread. Idle connections
//! observe the stop flag at packet boundaries via [`WireServerConfig::read_timeout`] without
//! needing step (3) at all, but a connection blocked mid-packet (some bytes of the current
//! packet already read: [`crate::codec::read_fully_or_stop`] never tears a packet, so it keeps
//! blocking past the stop flag until the peer sends the rest, or forever if the peer never
//! does) does need it: `shutdown(Shutdown::Both)` makes that blocked `read` return an error
//! immediately, and the connection thread's normal error-handling cleanup (rollback, then
//! `htap_server::Session::drop`) takes it from there.
//!
//! `live_connections` (a registry of `try_clone`d [`TcpStream`]s keyed by connection id) is the
//! only state step (3) needs. The accept-vs-shutdown race is closed from both directions: the
//! accept thread registers a new connection (before spawning its thread) and checks the stop
//! flag under the *same* lock, dropping the connection unregistered if shutdown has already
//! started; and `shutdown` itself joins the accept thread (step 2) before reading the registry
//! (step 3), so by the time it takes that snapshot, no further registration can race in. Every
//! registration is unregistered by an RAII guard ([`ConnectionGuard`]) that runs on every exit
//! path out of the connection thread, panics included.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use htap_common::types::{ColumnDef, DataType, Row, Value};
use htap_common::HtapError;
use htap_server::{LocalServer, Session as ServerSession};
use htap_sql::{CommandResult, QueryResult, StatementResult};
use parking_lot::Mutex;
use sqlparser::ast::Statement as SqlStatement;

use crate::binary_codec::{
    decode_execute, encode_binary_row, encode_stmt_prepare_ok, ParamType, ParamValue,
};
use crate::codec::{
    read_message_with_stop, read_u16, read_u32, write_lenenc_int, write_message, write_packet,
    SeqCounter, PACKET_TOO_LARGE_ERROR_KIND, SHUTDOWN_ERROR_KIND,
};
use crate::compression::{CompressedStream, CompressionAlgorithm};
use crate::error_map::{build_err_payload, map_htap_error};
use crate::handshake::{
    decode_ssl_request, generate_scramble, AuthSwitchRequest, ChangeUserRequest,
    HandshakeResponse41, HandshakeV10, SCRAMBLE_LEN,
};
use crate::prepared::{PreparedStatementRegistry, PreparedStmt, MAX_PREPARED_STATEMENTS};
use crate::proto::*;
use crate::result_codec::{
    build_column_def41, build_command_ok, build_resultset_terminator, encode_text_row,
};
use crate::sha1::verify_native_password;
use crate::shim::{is_accepted_schema, try_shim, ShimOutcome};
use crate::tls::{perform_tls_handshake, Conn, ReloadableCertResolver};
use rustls::server::ResolvesServerCert;

/// TLS certificate and private-key files used by the listener.
#[derive(Debug, Clone)]
pub struct TlsConfig {
    /// PEM-encoded server certificate chain.
    pub cert_path: PathBuf,
    /// PEM-encoded private key.
    pub key_path: PathBuf,
}

/// Listener configuration.
#[derive(Debug, Clone)]
pub struct WireServerConfig {
    /// Address to bind. Defaults to `127.0.0.1:3307`.
    pub listen: SocketAddr,
    /// Maximum number of simultaneously open connections; excess connections receive
    /// ERR 1040 and are closed.
    pub max_connections: usize,
    /// Password used to bootstrap the root account on first startup. `None` bootstraps root
    /// without a password.
    pub password: Option<String>,
    /// TLS certificate and key configuration. `None` disables TLS.
    pub tls: Option<TlsConfig>,
    /// Reject non-TLS authentication attempts.
    pub require_secure_transport: bool,
    /// Read timeout used to observe shutdown between packets.
    pub read_timeout: Duration,
    /// Maximum total size, in bytes, of one logical protocol message (a `COM_QUERY`'s SQL text,
    /// one row, an OK/ERR packet, ...) after multi-packet reassembly (Phase 11 plan task 8).
    /// Enforced on every message this server reads from a client; a message whose declared total
    /// length would exceed this answers `ER_NET_PACKET_TOO_LARGE` (1153) and closes the
    /// connection. Also reported dynamically as `@@max_allowed_packet` for every session opened
    /// against this server (`htap_server::Session::set_max_allowed_packet`). Defaults to MySQL's
    /// own default, 64 MiB (`htap_sql::DEFAULT_MAX_ALLOWED_PACKET`).
    pub max_allowed_packet: usize,

    /// Whether to enable negotiated protocol compression for client connections.
    pub compression_enabled: bool,
}

impl Default for WireServerConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 3307)),
            max_connections: 64,
            password: None,
            tls: None,
            require_secure_transport: false,
            read_timeout: Duration::from_millis(200),
            max_allowed_packet: htap_sql::DEFAULT_MAX_ALLOWED_PACKET as usize,
            compression_enabled: true,
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
    /// Registry of every currently open connection's socket, keyed by connection id (Phase 11
    /// plan task 11); see the module's `# Shutdown` docs.
    live_connections: Arc<Mutex<HashMap<u32, TcpStream>>>,
    shared: Arc<Shared>,
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
    tls_resolver: Option<Arc<ReloadableCertResolver>>,
    tls_server_config: Option<Arc<rustls::ServerConfig>>,
    tls_cert_paths: Option<(PathBuf, PathBuf)>,
    require_secure_transport: bool,
    stop: Arc<AtomicBool>,
    read_timeout: Duration,
    max_allowed_packet: usize,
    compression_enabled: bool,
    // `WireServer::start` always bootstraps this before accepting connections. The legacy
    // shared-password path remains only for manually constructed test/shared configurations.
    accounts_initialized: bool,
    next_connection_id: AtomicU32,
}

/// RAII guard that unregisters a connection from `live_connections` and decrements
/// `connection_count` (Phase 11 plan task 11 and the finding-4 fix pass) on every exit path out
/// of its connection thread, panics included: the guard lives in that thread's stack frame for
/// the whole call to [`handle_connection`], so its `Drop` runs whether that call returns
/// normally, returns an error, or the frame unwinds through a panic. Before this fix,
/// `connection_count`'s decrement lived in the spawned closure *after* `handle_connection`
/// returned, so a panic inside that call skipped it and leaked the count permanently (eventually
/// wedging `max_connections`); folding it into this guard's `Drop` closes that gap the same way
/// the registry removal already was closed.
struct ConnectionGuard {
    id: u32,
    registry: Arc<Mutex<HashMap<u32, TcpStream>>>,
    connection_count: Arc<AtomicUsize>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.registry.lock().remove(&self.id);
        self.connection_count.fetch_sub(1, Ordering::SeqCst);
    }
}

fn load_tls_certified_key(config: &TlsConfig) -> io::Result<rustls::sign::CertifiedKey> {
    ReloadableCertResolver::load(&config.cert_path, &config.key_path)
}

impl WireServer {
    /// Binds the listener and starts accepting connections.
    pub fn start(config: WireServerConfig, server: Arc<LocalServer>) -> io::Result<Self> {
        if config.require_secure_transport && config.tls.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "require_secure_transport requires TLS configuration",
            ));
        }
        let (tls_resolver, tls_cert_paths) = match config.tls.as_ref() {
            Some(tls) => (
                Some(Arc::new(ReloadableCertResolver::new(
                    load_tls_certified_key(tls)?,
                ))),
                Some((tls.cert_path.clone(), tls.key_path.clone())),
            ),
            None => (None, None),
        };
        let tls_server_config = match tls_resolver.as_ref() {
            Some(resolver) => Some(Arc::new(
                rustls::ServerConfig::builder_with_provider(Arc::new(
                    rustls::crypto::ring::default_provider(),
                ))
                .with_safe_default_protocol_versions()
                .map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid TLS protocol configuration: {e}"),
                    )
                })?
                .with_no_client_auth()
                .with_cert_resolver(Arc::clone(resolver) as Arc<dyn ResolvesServerCert>),
            )),
            None => None,
        };
        let bootstrap = server
            .bootstrap_root_account(config.password.as_deref())
            .map_err(|e| io::Error::other(format!("failed to bootstrap root account: {e}")))?;
        if bootstrap.config_password_matches_root == Some(false) {
            tracing::warn!(
                "configured password no longer controls root login; administrators should use \
                 ALTER USER root IDENTIFIED BY to change the persisted root password"
            );
        }
        let listener = TcpListener::bind(config.listen)?;
        listener.set_nonblocking(true)?;
        let local_addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let conn_threads: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
        let connection_count = Arc::new(AtomicUsize::new(0));
        let live_connections: Arc<Mutex<HashMap<u32, TcpStream>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let shared = Arc::new(Shared {
            server,
            password: config.password.clone(),
            tls_resolver,
            tls_server_config,
            tls_cert_paths,
            compression_enabled: config.compression_enabled,
            require_secure_transport: config.require_secure_transport,
            stop: Arc::clone(&stop),
            read_timeout: config.read_timeout,
            max_allowed_packet: config.max_allowed_packet,
            accounts_initialized: true,
            next_connection_id: AtomicU32::new(1),
        });
        let max_connections = config.max_connections.max(1);

        let accept_stop = Arc::clone(&stop);
        let accept_threads = Arc::clone(&conn_threads);
        let accept_count = Arc::clone(&connection_count);
        let accept_live_connections = Arc::clone(&live_connections);
        let accept_shared = Arc::clone(&shared);
        let accept_thread = std::thread::Builder::new()
            .name("htap-wire-accept".into())
            .spawn(move || {
                accept_loop(
                    listener,
                    accept_shared,
                    max_connections,
                    accept_stop,
                    accept_threads,
                    accept_count,
                    accept_live_connections,
                )
            })?;

        tracing::info!(%local_addr, "htap-wire listening");
        Ok(Self {
            local_addr,
            stop,
            accept_thread: Some(accept_thread),
            conn_threads,
            connection_count,
            live_connections,
            shared,
        })
    }

    /// Address the listener is bound to (useful with port 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Reloads the configured TLS certificate and private key for future TLS handshakes.
    ///
    /// Existing TLS connections retain the certificate negotiated when they connected.
    pub fn reload_tls_certs(&self) -> io::Result<()> {
        let resolver =
            self.shared.tls_resolver.as_ref().ok_or_else(|| {
                io::Error::new(io::ErrorKind::Unsupported, "TLS is not configured")
            })?;
        let (cert_path, key_path) = self.shared.tls_cert_paths.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "TLS certificate paths are not configured",
            )
        })?;
        resolver.reload(cert_path, key_path)
    }

    /// Number of currently open connections.
    pub fn connection_count(&self) -> usize {
        self.connection_count.load(Ordering::SeqCst)
    }

    /// Stops accepting, force-closes any connection blocked mid-packet, waits for every
    /// connection thread, and frees the port (Phase 11 plan task 11; see the module's
    /// `# Shutdown` docs for why each step must happen in this order).
    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.accept_thread.take() {
            let _ = h.join();
        }
        // No new connection can be registered past this point: the accept thread (just joined)
        // only ever registers a connection after checking `stop` under the same lock this takes,
        // and it has now observed `stop = true` for good (either on its very last iteration, if
        // one was in flight when `stop` was set above, or on every iteration since).
        let streams: Vec<TcpStream> = std::mem::take(&mut *self.live_connections.lock())
            .into_values()
            .collect();
        for stream in streams {
            let _ = stream.shutdown(Shutdown::Both);
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
    live_connections: Arc<Mutex<HashMap<u32, TcpStream>>>,
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
                let connection_id = shared.next_connection_id.fetch_add(1, Ordering::Relaxed);
                let cloned = match stream.try_clone() {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::error!(
                            connection_id,
                            error = %e,
                            "failed to clone connection stream for the shutdown registry"
                        );
                        continue;
                    }
                };
                // Register before spawning, and check `stop` under the same lock (Phase 11 plan
                // task 11): this is one half of closing the accept-vs-shutdown race, the other
                // half being `WireServer::shutdown` joining this very thread before it reads the
                // registry. If shutdown has already started, drop this connection unregistered
                // and unspawned rather than let it slip past the force-close step.
                {
                    let mut registry = live_connections.lock();
                    if stop.load(Ordering::SeqCst) {
                        drop(registry);
                        continue;
                    }
                    registry.insert(connection_id, cloned);
                }
                connection_count.fetch_add(1, Ordering::SeqCst);
                let shared = Arc::clone(&shared);
                let guard_registry = Arc::clone(&live_connections);
                let guard_count = Arc::clone(&connection_count);
                let spawn = std::thread::Builder::new()
                    .name(format!("htap-wire-conn-{connection_id}"))
                    .spawn(move || {
                        // `connection_count`'s decrement lives in this guard's `Drop`, not after
                        // `handle_connection` returns (finding 4): that runs even if
                        // `handle_connection` panics.
                        let _guard = ConnectionGuard {
                            id: connection_id,
                            registry: guard_registry,
                            connection_count: guard_count,
                        };
                        let result = handle_connection(stream, connection_id, &shared);
                        match result {
                            Ok(()) => tracing::debug!(connection_id, "connection closed"),
                            Err(e) => {
                                tracing::debug!(connection_id, error = %e, "connection ended")
                            }
                        }
                    });
                match spawn {
                    Ok(handle) => conn_threads.lock().push(handle),
                    Err(e) => {
                        connection_count.fetch_sub(1, Ordering::SeqCst);
                        // The thread never started, so its `ConnectionGuard` never ran: remove
                        // the registration ourselves.
                        live_connections.lock().remove(&connection_id);
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
    /// Capability flags requested by the client's handshake response, intersected with
    /// [`SERVER_CAPABILITIES`] — the capabilities actually in effect for this connection's whole
    /// lifetime (Phase 11 plan task 10 generalizes what was previously computed ad hoc per flag,
    /// as [`Self::deprecate_eof`] still is). New capability-gated behavior should check this
    /// field (see [`Self::multi_statements`]) rather than repeating the pattern inline.
    negotiated_capabilities: u32,
    /// Capability flags negotiated at this connection's initial handshake; `COM_CHANGE_USER`
    /// (Phase 11 plan task 7) is never itself a re-negotiation, so its request body is decoded
    /// using these same flags (see [`ChangeUserRequest::decode`]). Unlike
    /// `negotiated_capabilities`, this is the client's raw requested flags, not intersected with
    /// [`SERVER_CAPABILITIES`]: it only ever drives wire-layout decisions (which encoding a field
    /// uses), never a capability gate.
    capability_flags: u32,
    /// The scramble sent in this connection's initial handshake (or the one from a later auth
    /// switch during that handshake). Real clients (confirmed against `mysql-28.0.2`'s
    /// `exec_com_change_user`, which hashes `COM_CHANGE_USER`'s auth response against
    /// `self.0.nonce` — the *stored* handshake scramble, never a fresh one) compute
    /// `COM_CHANGE_USER`'s auth response against this same scramble unless the server sends a
    /// fresh `AuthSwitchRequest` for the occasion (see [`respond_change_user`]).
    scramble: [u8; SCRAMBLE_LEN],
}

impl Session {
    /// Whether this connection negotiated `CLIENT_MULTI_STATEMENTS` (Phase 11 plan task 10):
    /// gates whether `COM_QUERY` text containing more than one statement is honored
    /// ([`respond_query`]) rather than rejected exactly as it always was before this flag existed.
    fn multi_statements(&self) -> bool {
        self.negotiated_capabilities & CLIENT_MULTI_STATEMENTS != 0
    }
}

/// Verifies a `mysql_native_password` challenge/response against a configured credential.
///
/// This is retained only for the legacy manually constructed [`Shared`] test path. Normal
/// listeners authenticate against catalog accounts through [`LocalServer::authenticate_session`].
fn verify_credentials(
    scramble: &[u8; SCRAMBLE_LEN],
    configured_password: Option<&str>,
    auth_response: &[u8],
) -> bool {
    match configured_password {
        None => true,
        Some(pw) => verify_native_password(scramble, pw, auth_response),
    }
}

fn send(stream: &mut impl Write, seq: &mut SeqCounter, payload: &[u8]) -> io::Result<()> {
    write_message(stream, seq, payload)
}

/// Builds the `SERVER_STATUS_*` bitmask to report for this connection's current state (finding 8
/// of the Phase 11 fix pass): `SERVER_STATUS_AUTOCOMMIT` reflects the session's real `autocommit`
/// setting (`SET autocommit = 0` now actually clears it, instead of it being hardcoded on),
/// `SERVER_STATUS_IN_TRANS` is set whenever a transaction — explicit (`BEGIN`) or implicit
/// (autocommit off; `Session::in_transaction` already covers both) — is open, and
/// `SERVER_MORE_RESULTS_EXISTS` is set when `more_results` is (Phase 11 plan task 10, unchanged).
/// Used by every OK/EOF/terminator builder this module calls with a live `htap_server::Session`
/// on hand; the handful of call sites before one exists (the pre-auth handshake OK) or right
/// after one is destroyed have no session state to report and keep their own literal
/// `SERVER_STATUS_AUTOCOMMIT` (a session's default state is always autocommit-on, no open
/// transaction).
fn session_status_flags(server_session: &htap_server::Session, more_results: bool) -> u16 {
    let mut status = 0u16;
    if server_session.autocommit() {
        status |= SERVER_STATUS_AUTOCOMMIT;
    }
    if server_session.in_transaction() {
        status |= SERVER_STATUS_IN_TRANS;
    }
    if more_results {
        status |= SERVER_MORE_RESULTS_EXISTS;
    }
    status
}

fn send_err(
    stream: &mut impl Write,
    seq: &mut SeqCounter,
    (code, state): (u16, &str),
    message: &str,
) -> io::Result<()> {
    send(stream, seq, &build_err_payload(code, state, message))?;
    stream.flush()
}

/// Cap, in bytes, on any single pre-authentication read (the initial handshake response, and
/// either side of an auth-plugin switch, including one triggered by `COM_CHANGE_USER`): finding
/// 1 of the Phase 11 fix pass. A real handshake response is small — a few hundred bytes plus
/// whatever `CLIENT_CONNECT_ATTRS` connect attributes the client sends, which real drivers keep
/// to a handful of short key/value pairs (`_client_name`, `_client_version`, `_os`, `_pid`,
/// `_platform`, `program_name`; see `mysql-28.0.2`'s `connect_attrs` in
/// `src/conn/mod.rs`) — so 64 KiB is generous headroom over any legitimate client while capping
/// how much an unauthenticated peer can make this server allocate from a single declared length,
/// before a single byte of that length is trusted. Never larger than the server's configured
/// `max_allowed_packet`, in case that has been configured smaller than 64 KiB.
const AUTH_PHASE_MAX_PACKET: usize = 64 * 1024;

/// Reads a single pre-authentication message (the initial handshake response, or either side of
/// an auth-plugin switch: never chunked in practice, but framed at the message level via
/// [`read_message_with_stop`] regardless, exactly like [`read_command`]) bounded by
/// [`AUTH_PHASE_MAX_PACKET`] intersected with this server's configured `max_allowed_packet`.
///
/// Finding 1 of the Phase 11 fix pass: before this bound, `crate::codec::read_packet_with_stop`
/// allocated a buffer sized from the *declared* packet length — up to just under 16 MiB — before any
/// authentication had happened, so an unauthenticated peer could make every connection attempt
/// allocate up to that much memory merely by declaring a large length and never sending the
/// bytes. [`read_message_with_stop`] checks the declared length against `max_len` *before*
/// allocating (see its own doc comment), so the allocation this function can be made to perform
/// is capped at `max_len`, checked before it happens.
fn read(stream: &mut impl Read, stop: &AtomicBool, max_len: usize) -> io::Result<(u8, Vec<u8>)> {
    read_message_with_stop(stream, max_len, Some(stop))
}

/// [`AUTH_PHASE_MAX_PACKET`] intersected with this server's configured `max_allowed_packet`
/// (never larger than it, in case it was configured smaller).
fn auth_phase_max_len(shared: &Shared) -> usize {
    shared.max_allowed_packet.min(AUTH_PHASE_MAX_PACKET)
}

/// Reads one full command message (Phase 11 plan task 8), reassembling multi-packet payloads and
/// enforcing `max_allowed_packet`; see [`crate::codec::read_message_with_stop`].
fn read_command(
    stream: &mut impl Read,
    max_allowed_packet: usize,
    stop: &AtomicBool,
) -> io::Result<(u8, Vec<u8>)> {
    read_message_with_stop(stream, max_allowed_packet, Some(stop))
}

enum ServerStream {
    Plain(Conn),
    Compressed(CompressedStream<Conn>),
}

impl Read for ServerStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.read(buf),
            Self::Compressed(stream) => stream.read(buf),
        }
    }
}

impl Write for ServerStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.write(buf),
            Self::Compressed(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(stream) => stream.flush(),
            Self::Compressed(stream) => stream.flush(),
        }
    }
}

impl ServerStream {
    fn reset_sequence(&mut self) {
        match self {
            Self::Plain(_) => {}
            Self::Compressed(stream) => stream.reset_sequence(),
        }
    }
}

fn handle_connection(stream: TcpStream, connection_id: u32, shared: &Shared) -> io::Result<()> {
    stream.set_read_timeout(Some(shared.read_timeout))?;
    let mut stream = Conn::Plain(stream);
    let (mut session_struct, compression, mut server_session) =
        match authenticate(&mut stream, connection_id, shared)? {
            Some((s, c, server_session)) => (s, c, server_session),
            None => return Ok(()),
        };
    tracing::debug!(
        connection_id,
        deprecate_eof = session_struct.deprecate_eof,
        "authenticated"
    );

    let mut stream = match compression {
        Some(compression) => ServerStream::Compressed(CompressedStream::new(
            stream,
            Some(compression),
            shared.max_allowed_packet,
        )),
        None => ServerStream::Plain(stream),
    };

    // One `htap_server::Session` per authenticated connection (Phase 10 task 9): buffered
    // writes, autocommit state, and user/system variables all live here for the connection's
    // whole lifetime. One `PreparedStatementRegistry` per connection too (Phase 11 plan task 4):
    // unlike `htap_server::Session`, prepared statements have no counterpart inside `htap-server`
    // at all, since they are a wire-protocol concept.
    server_session.set_max_allowed_packet(shared.max_allowed_packet as u64);
    let mut registry =
        PreparedStatementRegistry::new(MAX_PREPARED_STATEMENTS, shared.max_allowed_packet);
    let result = run_commands(
        &mut stream,
        shared,
        &mut session_struct,
        &mut server_session,
        &mut registry,
    );
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
    stream: &mut ServerStream,
    shared: &Shared,
    session: &mut Session,
    server_session: &mut htap_server::Session,
    registry: &mut PreparedStatementRegistry,
) -> io::Result<()> {
    loop {
        stream.reset_sequence();
        let (pkt_seq, payload) = match read_command(stream, shared.max_allowed_packet, &shared.stop)
        {
            Ok(p) => p,
            Err(e) if e.kind() == SHUTDOWN_ERROR_KIND => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) if e.kind() == PACKET_TOO_LARGE_ERROR_KIND => {
                // Best-effort: reply with ER_NET_PACKET_TOO_LARGE (1153), then close the
                // connection regardless of whether the reply itself succeeds. The exact
                // sequence id used here is necessarily approximate (the caller never learns
                // how many chunks of the oversize message it had already started reassembling
                // when the limit was hit): every command this loop reads starts a fresh
                // sequence at 0, so 1 is the id a well-behaved single-packet response would
                // use, and the connection closes immediately after regardless.
                let mut seq = SeqCounter::new();
                seq.continue_after(0);
                let _ = send_err(
                    stream,
                    &mut seq,
                    ER_NET_PACKET_TOO_LARGE,
                    &format!(
                        "message exceeds max_allowed_packet ({} bytes)",
                        shared.max_allowed_packet
                    ),
                );
                stream.flush()?;
                return Ok(());
            }
            // Framing errors (e.g. a sequence-id mismatch across chunks) desynchronize the
            // stream: close.
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
            COM_PING => send(
                stream,
                &mut seq,
                &build_command_ok(0, "", session_status_flags(server_session, false)),
            )?,
            COM_INIT_DB => {
                let name = String::from_utf8_lossy(body).into_owned();
                respond_use_db(stream, &mut seq, &name, server_session)?;
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
            COM_STMT_PREPARE => {
                respond_stmt_prepare(stream, &mut seq, body, session, server_session, registry)?;
            }
            COM_STMT_EXECUTE => {
                respond_stmt_execute(stream, &mut seq, body, session, server_session, registry)?;
            }
            COM_STMT_CLOSE => {
                // No response, by protocol; an unknown id is silently ignored.
                if let Ok(stmt_id) = read_u32(body, &mut 0) {
                    registry.close(stmt_id);
                }
            }
            COM_STMT_RESET => {
                respond_stmt_reset(stream, &mut seq, body, registry, server_session)?;
            }
            COM_STMT_SEND_LONG_DATA => {
                // No response, by protocol, on any path (decision 5: errors poison the
                // statement and surface at the next EXECUTE instead).
                let mut pos = 0;
                if let (Ok(stmt_id), Ok(param_id)) =
                    (read_u32(body, &mut pos), read_u16(body, &mut pos))
                {
                    registry.append_long_data(stmt_id, param_id, &body[pos..]);
                }
            }
            COM_STMT_FETCH => {
                send_err(
                    stream,
                    &mut seq,
                    ER_UNKNOWN,
                    "COM_STMT_FETCH (server-side cursors) is not supported",
                )?;
            }
            COM_RESET_CONNECTION => {
                respond_reset_connection(stream, &mut seq, server_session, registry)?;
            }
            COM_CHANGE_USER => {
                let keep_open = respond_change_user(
                    stream,
                    &mut seq,
                    body,
                    shared,
                    session,
                    server_session,
                    registry,
                )?;
                if !keep_open {
                    stream.flush()?;
                    return Ok(());
                }
            }
            _ => {
                send_err(
                    stream,
                    &mut seq,
                    ER_UNKNOWN_COMMAND,
                    &format!("Unknown command 0x{command:02x}"),
                )?;
            }
        }
        stream.flush()?;
    }
}

fn advertised_capabilities(shared: &Shared) -> u32 {
    let mut capabilities = SERVER_CAPABILITIES;
    if shared.compression_enabled {
        capabilities |= CLIENT_COMPRESS | CLIENT_ZSTD_COMPRESSION_ALGORITHM;
    }
    if shared.tls_resolver.is_some() {
        capabilities |= CLIENT_SSL;
    }
    capabilities
}

fn authenticate(
    stream: &mut Conn,
    connection_id: u32,
    shared: &Shared,
) -> io::Result<Option<(Session, Option<CompressionAlgorithm>, ServerSession)>> {
    let mut seq = SeqCounter::new();
    let mut scramble = generate_scramble()?;
    send(
        stream,
        &mut seq,
        &HandshakeV10 {
            capabilities: advertised_capabilities(shared),
            ..HandshakeV10::new(connection_id, scramble)
        }
        .encode(),
    )?;

    let auth_max_len = auth_phase_max_len(shared);
    let (pkt_seq, mut payload) = match read(stream, &shared.stop, auth_max_len) {
        Ok(p) => p,
        Err(e) if e.kind() == SHUTDOWN_ERROR_KIND => return Ok(None),
        // An oversize handshake response (finding 1): close the connection without ever having
        // buffered its declared length, exactly like every other pre-auth framing error.
        Err(e) => return Err(e),
    };
    seq.continue_after(pkt_seq);

    let mut secure_transport = false;
    match HandshakeResponse41::peek_capabilities(&payload) {
        Some(caps) if caps & CLIENT_SSL != 0 => {
            // The resolver establishes that TLS credentials are configured; the shared server
            // config uses that resolver so future handshakes see reloaded certificates.
            if shared.tls_resolver.is_none() {
                send_err(
                    stream,
                    &mut seq,
                    ER_UNKNOWN,
                    "TLS is not supported by this server",
                )?;
                return Ok(None);
            }
            let Some(tls_server_config) = shared.tls_server_config.as_ref() else {
                send_err(
                    stream,
                    &mut seq,
                    ER_UNKNOWN,
                    "TLS is not supported by this server",
                )?;
                return Ok(None);
            };
            if let Err(e) = decode_ssl_request(&payload) {
                send_err(
                    stream,
                    &mut seq,
                    ER_UNKNOWN,
                    &format!("Malformed SSL request: {e}"),
                )?;
                return Ok(None);
            }
            let conn =
                rustls::ServerConnection::new(Arc::clone(tls_server_config)).map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("failed to create TLS server connection: {e}"),
                    )
                })?;
            let tcp = stream.tcp().try_clone()?;
            *stream = perform_tls_handshake(&mut tcp.try_clone()?, conn, &shared.stop)?;
            secure_transport = true;
            let (response_seq, response_payload) = match read(stream, &shared.stop, auth_max_len) {
                Ok(p) => p,
                Err(e) if e.kind() == SHUTDOWN_ERROR_KIND => return Ok(None),
                Err(e) => return Err(e),
            };
            seq.continue_after(response_seq);
            payload = response_payload;
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

    if shared.require_secure_transport && !secure_transport {
        send_err(
            stream,
            &mut seq,
            ER_ACCESS_DENIED,
            "Secure transport required",
        )?;
        return Ok(None);
    }

    let mut auth_response = response.auth_response.clone();
    let needs_switch = match response.auth_plugin.as_deref() {
        Some(p) => !p.is_empty() && p != AUTH_PLUGIN_NATIVE,
        None => false,
    };
    if needs_switch {
        scramble = generate_scramble()?;
        let switch = AuthSwitchRequest {
            plugin: AUTH_PLUGIN_NATIVE.into(),
            scramble,
        };
        send(stream, &mut seq, &switch.encode())?;
        let (pkt_seq, switch_payload) = match read(stream, &shared.stop, auth_max_len) {
            Ok(p) => p,
            Err(e) if e.kind() == SHUTDOWN_ERROR_KIND => return Ok(None),
            Err(e) => return Err(e),
        };
        seq.continue_after(pkt_seq);
        auth_response = switch_payload;
    }

    let server_session = if shared.accounts_initialized {
        match shared
            .server
            .authenticate_session(&response.username, &scramble, &auth_response)
        {
            Ok(session) => session,
            Err(_) => {
                tracing::warn!(connection_id, user = %response.username, "access denied");
                send_err(
                    stream,
                    &mut seq,
                    ER_ACCESS_DENIED,
                    &format!("Access denied for user '{}'", response.username),
                )?;
                return Ok(None);
            }
        }
    } else {
        // `WireServer::start` bootstraps catalog accounts before listening, so this legacy
        // shared-password fallback is unreachable in normal operation.
        let authorized = verify_credentials(
            &scramble_array(&scramble),
            shared.password.as_deref(),
            &auth_response,
        );
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
        shared.server.open_session()
    };

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

    // No `htap_server::Session` exists yet at this point (see `session_status_flags`'s doc
    // comment): a fresh session's default state is always autocommit-on with no open
    // transaction, so that literal status is exactly right here.
    send(
        stream,
        &mut seq,
        &build_command_ok(0, "", SERVER_STATUS_AUTOCOMMIT),
    )?;
    stream.flush()?;
    tracing::debug!(connection_id, user = %response.username, "connection authenticated");
    let negotiated_capabilities = response.capability_flags & SERVER_CAPABILITIES;
    let compression = if shared.compression_enabled
        && response.capability_flags & CLIENT_ZSTD_COMPRESSION_ALGORITHM != 0
    {
        Some(CompressionAlgorithm::Zstd {
            level: response.zstd_compression_level.unwrap_or(3).clamp(1, 3) as i32,
        })
    } else if shared.compression_enabled && response.capability_flags & CLIENT_COMPRESS != 0 {
        Some(CompressionAlgorithm::Zlib)
    } else {
        None
    };

    Ok(Some((
        Session {
            deprecate_eof: negotiated_capabilities & CLIENT_DEPRECATE_EOF != 0,
            negotiated_capabilities,
            capability_flags: response.capability_flags,
            scramble,
        },
        compression,
        server_session,
    )))
}

fn scramble_array(s: &[u8; SCRAMBLE_LEN]) -> [u8; SCRAMBLE_LEN] {
    *s
}

fn respond_use_db(
    stream: &mut ServerStream,
    seq: &mut SeqCounter,
    name: &str,
    server_session: &htap_server::Session,
) -> io::Result<()> {
    if is_accepted_schema(name) {
        send(
            stream,
            seq,
            &build_command_ok(0, "", session_status_flags(server_session, false)),
        )
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

/// Encodes one complete result set (column count, column defs, an optional legacy mid-
/// terminator, rows via `encode_row`, and a final terminator) as packets appended to `out`.
///
/// Shared by the text protocol ([`encode_statement_result`]'s `Query` arm, via
/// [`crate::result_codec::encode_text_row`]) and the binary protocol
/// ([`respond_stmt_execute`]'s query-result arm, via
/// [`crate::binary_codec::encode_binary_row`]): both shapes differ only in how a row's bytes are
/// encoded, never in packet sequencing or terminator placement.
fn encode_resultset(
    out: &mut Vec<u8>,
    seq: &mut SeqCounter,
    columns: &[ColumnDef],
    rows: &[Row],
    deprecate_eof: bool,
    status: u16,
    encode_row: impl Fn(&Row, &[ColumnDef]) -> io::Result<Vec<u8>>,
) -> io::Result<()> {
    let mut count = Vec::with_capacity(9);
    write_lenenc_int(&mut count, columns.len() as u64);
    write_message(out, seq, &count)?;
    for col in columns {
        write_message(out, seq, &build_column_def41(col))?;
    }
    if !deprecate_eof {
        // The legacy mid-resultset terminator marks the end of the column-definitions block,
        // not the end of a result: it never carries `SERVER_MORE_RESULTS_EXISTS`, regardless of
        // `status` (Phase 11 plan task 10), though it does still carry the session's real
        // AUTOCOMMIT/IN_TRANS bits (Phase 11 fix pass, finding 8).
        write_message(
            out,
            seq,
            &build_resultset_terminator(false, status & !SERVER_MORE_RESULTS_EXISTS),
        )?;
    }
    for row in rows {
        // A single row's encoded bytes (e.g. one very large string/blob value) can exceed 16MB;
        // `write_message` splits it across as many physical packets as needed (Phase 11 plan
        // task 8), unlike `write_packet`, which would simply error. `encode_row` itself can also
        // fail (finding 6: a binary-protocol `Value::Timestamp` outside the wire format's
        // `0..=9999` year range); that error propagates from here exactly like an I/O error.
        write_message(out, seq, &encode_row(row, columns)?)?;
    }
    write_message(out, seq, &build_resultset_terminator(deprecate_eof, status))
}

/// Encodes a complete response to a query as packets appended to `out`. `status` is the full
/// `SERVER_STATUS_*` bitmask to report (Phase 11 fix pass, finding 8; see
/// `session_status_flags`), including `SERVER_MORE_RESULTS_EXISTS` when this is not the last
/// result of a `CLIENT_MULTI_STATEMENTS` batch (Phase 11 plan task 10).
pub fn encode_statement_result(
    out: &mut Vec<u8>,
    seq: &mut SeqCounter,
    result: &StatementResult,
    deprecate_eof: bool,
    status: u16,
) -> io::Result<()> {
    match result {
        StatementResult::Command(cmd) => {
            let info = command_info(cmd);
            write_message(out, seq, &build_command_ok(cmd.affected(), &info, status))
        }
        StatementResult::Query(qr) => encode_resultset(
            out,
            seq,
            &qr.columns,
            &qr.rows,
            deprecate_eof,
            status,
            |row, _columns| Ok(encode_text_row(row)),
        ),
    }
}

/// Handles `COM_QUERY`.
///
/// The shim ([`try_shim`]) is only ever tried against the *whole* SQL text, so it only ever
/// matches a single statement (see [`try_shim`]'s own doc comment on why an embedded `;` always
/// disables it). If the shim does not match and this connection negotiated
/// `CLIENT_MULTI_STATEMENTS` (Phase 11 plan task 10), the text is split with
/// [`htap_sql::parse_many`] and every statement is run in turn through
/// [`htap_server::Session::execute_statement`], stopping at the first error (including
/// `DurablePending`/`RecoveryRequired`, which are ordinary errors from that method's point of
/// view) and sending it as the final packet; every result but the last carries
/// `SERVER_MORE_RESULTS_EXISTS`. Without that capability, behavior is unchanged from before this
/// task: `server_session.execute(sql)` itself rejects multi-statement text via `parse_one`.
fn respond_query(
    stream: &mut ServerStream,
    seq: &mut SeqCounter,
    sql: &str,
    session: &Session,
    server_session: &mut htap_server::Session,
) -> io::Result<()> {
    if let Some(shim) = try_shim(sql) {
        let outcome: Result<StatementResult, HtapError> = match shim {
            ShimOutcome::Ok => Ok(StatementResult::ddl(0)),
            ShimOutcome::Rows { columns, rows } => Ok(StatementResult::query(columns, rows)),
            ShimOutcome::UseDb(name) => return respond_use_db(stream, seq, &name, server_session),
        };
        let status = session_status_flags(server_session, false);
        return write_query_response(stream, seq, outcome, session.deprecate_eof, status);
    }

    if !session.multi_statements() {
        let outcome = server_session.execute(sql);
        // Computed *after* `execute`: a statement can itself change autocommit/transaction state
        // (`BEGIN`, `COMMIT`, `SET autocommit = ...`), and the status reported must reflect the
        // state that statement left the session in, exactly like a real MySQL server.
        let status = session_status_flags(server_session, false);
        return write_query_response(stream, seq, outcome, session.deprecate_eof, status);
    }

    let statements = match htap_sql::parse_many(sql) {
        Ok(s) => s,
        Err(err) => {
            let status = session_status_flags(server_session, false);
            return write_query_response(stream, seq, Err(err), session.deprecate_eof, status);
        }
    };
    let last = statements.len() - 1;
    for (i, statement) in statements.into_iter().enumerate() {
        let outcome = server_session.execute_statement(statement);
        let is_err = outcome.is_err();
        // An error is always this batch's final packet (we stop right after), so it never
        // carries `SERVER_MORE_RESULTS_EXISTS` even when earlier statements remain unexecuted.
        let more_results = i != last && !is_err;
        let status = session_status_flags(server_session, more_results);
        write_query_response(stream, seq, outcome, session.deprecate_eof, status)?;
        if is_err {
            // Stop at the first error (including `DurablePending`/`RecoveryRequired`): the ERR
            // packet just sent is the batch's final packet.
            break;
        }
    }
    Ok(())
}

/// Encodes and sends one query response (OK/resultset or ERR), used by both the single-statement
/// and `CLIENT_MULTI_STATEMENTS` batch paths of [`respond_query`]. `status` is the full
/// `SERVER_STATUS_*` bitmask to report on success (Phase 11 fix pass, finding 8; see
/// `session_status_flags`).
fn write_query_response(
    stream: &mut ServerStream,
    seq: &mut SeqCounter,
    outcome: Result<StatementResult, HtapError>,
    deprecate_eof: bool,
    status: u16,
) -> io::Result<()> {
    let mut out = Vec::new();
    match outcome {
        Ok(result) => encode_statement_result(&mut out, seq, &result, deprecate_eof, status)?,
        Err(err) => {
            // Real ERR packets carry no status-flags field at all, so `status` never applies
            // here regardless of the caller's value (an error is always this batch's final
            // packet anyway; see `respond_query`).
            let (code, state) = map_htap_error(&err);
            tracing::debug!(code, error = %err, "statement failed");
            write_message(
                &mut out,
                seq,
                &build_err_payload(code, state, &err.to_string()),
            )?;
        }
    }
    stream.write_all(&out)?;
    stream.flush()
}

// -------------------------------------------------------------------------------------------
// Phase 11 plan task 4: COM_STMT_PREPARE / EXECUTE / CLOSE / RESET / SEND_LONG_DATA / FETCH.
// -------------------------------------------------------------------------------------------

/// Whether `statement` is one this server can prepare: `INSERT`, `UPDATE`, `DELETE`, or a
/// `SELECT`-shaped query. `SET`, transaction control, DDL, and `SHOW`/`DESCRIBE` are rejected as
/// [`HtapError::Unsupported`] (none of them can usefully be re-executed with bound parameters,
/// and several — `SET`, transaction control — have no placeholder-bearing positions at all).
fn check_preparable(statement: &SqlStatement) -> Result<(), HtapError> {
    match statement {
        SqlStatement::Insert(_)
        | SqlStatement::Update(_)
        | SqlStatement::Delete(_)
        | SqlStatement::Query(_) => Ok(()),
        _ => Err(HtapError::Unsupported(
            "PREPARE only supports INSERT, UPDATE, DELETE, and SELECT/query statements".into(),
        )),
    }
}

/// The generic `ColumnDefinition41` MySQL sends for every `?` placeholder in a
/// `COM_STMT_PREPARE_OK` response: parameter types are not known ahead of binding, so every
/// parameter is described identically (`VAR_STRING`, nullable), exactly like a real MySQL server.
fn generic_param_column_def() -> ColumnDef {
    ColumnDef {
        name: "?".into(),
        data_type: DataType::String,
        nullable: true,
        primary_key: false,
    }
}

fn respond_stmt_prepare(
    stream: &mut ServerStream,
    seq: &mut SeqCounter,
    body: &[u8],
    session: &Session,
    server_session: &mut htap_server::Session,
    registry: &mut PreparedStatementRegistry,
) -> io::Result<()> {
    let sql = match std::str::from_utf8(body) {
        Ok(s) => s,
        Err(_) => {
            return send_err(
                stream,
                seq,
                ER_UNKNOWN,
                "COM_STMT_PREPARE payload is not valid UTF-8",
            )
        }
    };

    let outcome: Result<(u32, u16, Option<Vec<ColumnDef>>), HtapError> = (|| {
        // `htap_sql::parse_one` already rejects multi-statement text (`InvalidArgument`), so a
        // multi-statement `PREPARE` is rejected the same way regardless of whether the
        // connection ever negotiates `CLIENT_MULTI_STATEMENTS` (Phase 11 plan task 10, out of
        // this task's scope).
        let statement = htap_sql::parse_one(sql)?;
        check_preparable(&statement)?;
        let num_params = htap_sql::checked_placeholder_count(sql, &statement)?;
        // PREPARE checks catalog visibility before registration, just as EXECUTE enforces
        // privileges through `Session::execute_statement`.
        let catalog = server_session.check_statement_visible(&statement)?;
        let num_params = u16::try_from(num_params).map_err(|_| {
            HtapError::Unsupported(format!(
                "too many placeholders ({num_params}) in a single prepared statement"
            ))
        })?;
        let output_schema = htap_sql::resolve_prepare_output_schema(&statement, &catalog)?;
        let stmt = PreparedStmt::new(statement, num_params, output_schema.clone());
        let stmt_id = registry.insert(stmt).map_err(HtapError::Unsupported)?;
        Ok((stmt_id, num_params, output_schema))
    })();

    match outcome {
        Ok((stmt_id, num_params, output_schema)) => {
            let num_columns = output_schema.as_ref().map_or(0, Vec::len) as u16;
            let mut out = Vec::new();
            write_message(
                &mut out,
                seq,
                &encode_stmt_prepare_ok(stmt_id, num_columns, num_params, 0),
            )?;
            // A real client (`mysql_common`'s `_true_prepare`) always reads exactly one
            // terminator packet after a non-empty parameter/column block regardless of
            // `CLIENT_DEPRECATE_EOF` (`mysql-28.0.2/src/conn/mod.rs`'s unconditional
            // `self.drop_packet()` after each loop): unlike an ordinary resultset's *mid*-EOF,
            // there is no data for a client to read this block, so it is not omitted in
            // deprecated mode, only reshaped into the OK-shaped body
            // `build_resultset_terminator(true)` produces.
            if num_params > 0 {
                for _ in 0..num_params {
                    write_message(
                        &mut out,
                        seq,
                        &build_column_def41(&generic_param_column_def()),
                    )?;
                }
                write_message(
                    &mut out,
                    seq,
                    &build_resultset_terminator(
                        session.deprecate_eof,
                        session_status_flags(server_session, false),
                    ),
                )?;
            }
            if let Some(columns) = &output_schema {
                if !columns.is_empty() {
                    for col in columns {
                        write_message(&mut out, seq, &build_column_def41(col))?;
                    }
                    write_message(
                        &mut out,
                        seq,
                        &build_resultset_terminator(
                            session.deprecate_eof,
                            session_status_flags(server_session, false),
                        ),
                    )?;
                }
            }
            stream.write_all(&out)?;
            stream.flush()
        }
        Err(err) => {
            let (code, state) = map_htap_error(&err);
            tracing::debug!(code, error = %err, "PREPARE failed");
            send_err(stream, seq, (code, state), &err.to_string())
        }
    }
}

/// Whether `mysql_type` is one of the textual `COM_STMT_EXECUTE` parameter types
/// (`decode_param_value`'s lenenc-string arm), used to decide how a long-data parameter's
/// buffered bytes convert to an engine [`Value`].
fn is_textual_param_type(mysql_type: u8) -> bool {
    matches!(
        mysql_type,
        MYSQL_TYPE_VARCHAR
            | MYSQL_TYPE_VAR_STRING
            | MYSQL_TYPE_STRING
            | MYSQL_TYPE_ENUM
            | MYSQL_TYPE_SET
    )
}

fn respond_stmt_execute(
    stream: &mut ServerStream,
    seq: &mut SeqCounter,
    body: &[u8],
    session: &Session,
    server_session: &mut htap_server::Session,
    registry: &mut PreparedStatementRegistry,
) -> io::Result<()> {
    let mut pos = 0usize;
    let stmt_id = match read_u32(body, &mut pos) {
        Ok(v) => v,
        Err(_) => {
            return send_err(
                stream,
                seq,
                ER_UNKNOWN,
                "malformed COM_STMT_EXECUTE payload: missing statement id",
            )
        }
    };

    let Some(prepared) = registry.get(stmt_id) else {
        return send_err(
            stream,
            seq,
            ER_UNKNOWN_STMT_HANDLER,
            &format!("Unknown prepared statement handler ({stmt_id}) given to mysql_stmt_execute"),
        );
    };
    if let Some((code, state, message)) = prepared.poisoned.clone() {
        return send_err(stream, seq, (code, state), &message);
    }
    let num_params = prepared.num_params;
    let cached_types: Option<Vec<ParamType>> = prepared.cached_types.clone();
    let long_data_pending = prepared.long_data_pending();

    let decoded = match decode_execute(
        body,
        num_params,
        cached_types.as_deref(),
        &long_data_pending,
    ) {
        Ok(d) => d,
        Err(e) => {
            return send_err(
                stream,
                seq,
                ER_UNKNOWN,
                &format!("malformed COM_STMT_EXECUTE payload: {e}"),
            )
        }
    };

    // Update the cached parameter types unconditionally: `decoded.types` already reflects either
    // the freshly decoded types (flag = 1) or the echoed-back cache (flag = 0), per amendment A2.
    if let Some(stmt) = registry.get_mut(stmt_id) {
        stmt.cached_types = Some(decoded.types.clone());
    }

    // `ParamValue::DecimalText` substitutes directly as a numeric literal (finding 2 of the
    // Phase 11 fix pass) via `htap_sql::ParamLiteral::NumericText`, never through the lossy
    // `f64`/`i64` round trip an earlier version of this function used: what the binder then does
    // with a long/high-precision literal bound to an `Int64` or `Float64` column (this engine has
    // no dedicated DECIMAL type; see `docs/LIMITATIONS.md`) is ordinary binder behavior, the same
    // as for the identical literal typed directly into SQL text.
    let mut params: Vec<htap_sql::ParamLiteral> = Vec::with_capacity(decoded.values.len());
    for (i, pv) in decoded.values.into_iter().enumerate() {
        let param = match pv {
            ParamValue::Value(v) => htap_sql::ParamLiteral::Value(v),
            ParamValue::DecimalText(text) => htap_sql::ParamLiteral::NumericText(text),
            ParamValue::LongDataPlaceholder => {
                let bytes = registry
                    .get(stmt_id)
                    .and_then(|s| s.long_data.get(&(i as u16)))
                    .cloned()
                    .unwrap_or_default();
                let textual = decoded
                    .types
                    .get(i)
                    .is_some_and(|t| is_textual_param_type(t.mysql_type));
                let value = if textual {
                    match String::from_utf8(bytes) {
                        Ok(s) => Value::String(s),
                        Err(e) => Value::Bytes(e.into_bytes()),
                    }
                } else {
                    Value::Bytes(bytes)
                };
                htap_sql::ParamLiteral::Value(value)
            }
        };
        params.push(param);
    }

    // Long data has now been fully consumed into `params`; clear it before executing so a
    // later `EXECUTE` of the same statement (without a fresh `SEND_LONG_DATA`) does not resend
    // stale bytes for the same parameter index.
    registry.clear_long_data(stmt_id);

    let mut stmt_clone = match registry.get(stmt_id) {
        Some(stmt) => stmt.statement.clone(),
        None => {
            // Cannot happen: `registry.get(stmt_id)` succeeded above and nothing between here
            // and there can close this statement (single-threaded per connection).
            return send_err(
                stream,
                seq,
                ER_UNKNOWN_STMT_HANDLER,
                &format!(
                    "Unknown prepared statement handler ({stmt_id}) given to mysql_stmt_execute"
                ),
            );
        }
    };
    if let Err(e) = htap_sql::substitute_placeholders_ext(&mut stmt_clone, &params) {
        let (code, state) = map_htap_error(&e);
        return send_err(stream, seq, (code, state), &e.to_string());
    }

    // Privileges are enforced by `Session::execute_statement` for every prepared execution.
    let outcome = server_session.execute_statement(stmt_clone);
    // Computed after `execute_statement`, exactly like `respond_query`: a statement can itself
    // change autocommit/transaction state.
    let status = session_status_flags(server_session, false);
    let mut out = Vec::new();
    match outcome {
        Ok(StatementResult::Command(cmd)) => {
            let info = command_info(&cmd);
            write_message(
                &mut out,
                seq,
                &build_command_ok(cmd.affected(), &info, status),
            )?;
        }
        Ok(StatementResult::Query(qr)) => {
            // `encode_execute_query_result` only ever writes into `out` (never straight to
            // `stream`), so an encoding failure partway through (finding 6: a bound
            // `Value::Timestamp` outside the wire format's `0..=9999` year range) can still be
            // turned into a clean ERR response for *this* result instead of a connection-level
            // error: nothing has reached the wire yet, so both `out` and `seq` are rewound to
            // their pre-attempt state before building that ERR packet from scratch.
            let seq_before_result = *seq;
            if let Err(e) =
                encode_execute_query_result(&mut out, seq, &qr, session.deprecate_eof, status)
            {
                *seq = seq_before_result;
                out.clear();
                tracing::debug!(error = %e, "EXECUTE result encoding failed");
                write_message(
                    &mut out,
                    seq,
                    &build_err_payload(
                        ER_UNKNOWN.0,
                        ER_UNKNOWN.1,
                        &format!("failed to encode result row: {e}"),
                    ),
                )?;
            }
        }
        Err(err) => {
            let (code, state) = map_htap_error(&err);
            tracing::debug!(code, error = %err, "EXECUTE failed");
            write_message(
                &mut out,
                seq,
                &build_err_payload(code, state, &err.to_string()),
            )?;
        }
    }
    stream.write_all(&out)?;
    stream.flush()
}

/// Encodes a `COM_STMT_EXECUTE` query result as a binary resultset, identical in packet shape to
/// [`encode_statement_result`]'s text-protocol `Query` arm (see [`encode_resultset`]) but with
/// each row encoded via [`crate::binary_codec::encode_binary_row`].
fn encode_execute_query_result(
    out: &mut Vec<u8>,
    seq: &mut SeqCounter,
    qr: &QueryResult,
    deprecate_eof: bool,
    status: u16,
) -> io::Result<()> {
    // `COM_STMT_EXECUTE` is never part of a `CLIENT_MULTI_STATEMENTS` batch (that capability
    // only ever applies to `COM_QUERY` text), so `status` never needs `SERVER_MORE_RESULTS_EXISTS`
    // here, but it still carries the session's real AUTOCOMMIT/IN_TRANS bits (finding 8).
    encode_resultset(
        out,
        seq,
        &qr.columns,
        &qr.rows,
        deprecate_eof,
        status,
        encode_binary_row,
    )
}

fn respond_stmt_reset(
    stream: &mut ServerStream,
    seq: &mut SeqCounter,
    body: &[u8],
    registry: &mut PreparedStatementRegistry,
    server_session: &htap_server::Session,
) -> io::Result<()> {
    let stmt_id = match read_u32(body, &mut 0) {
        Ok(v) => v,
        Err(_) => {
            return send_err(
                stream,
                seq,
                ER_UNKNOWN,
                "malformed COM_STMT_RESET payload: missing statement id",
            )
        }
    };
    if registry.get(stmt_id).is_none() {
        return send_err(
            stream,
            seq,
            ER_UNKNOWN_STMT_HANDLER,
            &format!("Unknown prepared statement handler ({stmt_id}) given to mysql_stmt_reset"),
        );
    }
    registry.reset_statement(stmt_id);
    send(
        stream,
        seq,
        &build_command_ok(0, "", session_status_flags(server_session, false)),
    )
}

// -------------------------------------------------------------------------------------------
// Phase 11 plan task 6: COM_RESET_CONNECTION.
// -------------------------------------------------------------------------------------------

fn respond_reset_connection(
    stream: &mut ServerStream,
    seq: &mut SeqCounter,
    server_session: &mut htap_server::Session,
    registry: &mut PreparedStatementRegistry,
) -> io::Result<()> {
    match server_session.reset() {
        Ok(()) => {
            registry.clear();
            // Post-reset state: autocommit back on, no open transaction (`Session::reset`'s own
            // contract), so this reports plain `SERVER_STATUS_AUTOCOMMIT`.
            send(
                stream,
                seq,
                &build_command_ok(0, "", session_status_flags(server_session, false)),
            )
        }
        Err(err) => {
            // The session stays quarantined (`CommitOutcomePending`) and the registry is left
            // untouched: `Session::reset` changed nothing, so neither does this.
            let (code, state) = map_htap_error(&err);
            send_err(stream, seq, (code, state), &err.to_string())
        }
    }
}

// -------------------------------------------------------------------------------------------
// Phase 11 plan task 7: COM_CHANGE_USER.
// -------------------------------------------------------------------------------------------

/// Handles a `COM_CHANGE_USER` request. Returns `Ok(true)` to keep the connection open (success,
/// or an authenticated-but-quarantined failure), `Ok(false)` if the caller must close the
/// connection (malformed request or access denied).
fn respond_change_user(
    stream: &mut ServerStream,
    seq: &mut SeqCounter,
    body: &[u8],
    shared: &Shared,
    session: &mut Session,
    server_session: &mut htap_server::Session,
    registry: &mut PreparedStatementRegistry,
) -> io::Result<bool> {
    let request = match ChangeUserRequest::decode(body, session.capability_flags) {
        Ok(r) => r,
        Err(e) => {
            send_err(
                stream,
                seq,
                ER_ACCESS_DENIED,
                &format!("Malformed COM_CHANGE_USER request: {e}"),
            )?;
            return Ok(false);
        }
    };
    if let Some(db) = request.database.as_deref() {
        if !db.is_empty() && !is_accepted_schema(db) {
            send_err(stream, seq, ER_BAD_DB, &format!("Unknown database '{db}'"))?;
            return Ok(false);
        }
    }

    // Unless an auth-plugin switch is needed, a real client hashes its `COM_CHANGE_USER` auth
    // response against *this connection's original handshake scramble*, never a fresh one (see
    // `Session::scramble`'s doc comment) — there is no new challenge here to invalidate a replay
    // of an already-captured response, but that is exactly what real `mysql_native_password`
    // COM_CHANGE_USER does, and a real driver would fail to authenticate against anything else.
    let mut scramble = session.scramble;
    let mut auth_response = request.auth_response.clone();
    let needs_switch = match request.auth_plugin.as_deref() {
        Some(p) => !p.is_empty() && p != AUTH_PLUGIN_NATIVE,
        None => false,
    };
    if needs_switch {
        scramble = generate_scramble()?;
        // Persist the fresh scramble on the connection's `Session` right away (finding 3 of the
        // Phase 11 fix pass): confirmed against `mysql-28.0.2`'s `continue_auth`, a real client
        // updates its own stored nonce (`self.0.nonce`) the moment it receives this
        // `AuthSwitchRequest` (`src/conn/mod.rs` line ~672), before it even knows whether the
        // resulting authentication succeeds — and, critically, *keeps using that same nonce* for
        // a later `COM_CHANGE_USER` that itself needs no switch. Before this fix, this function
        // only ever hashed against `session.scramble` (the connection's original handshake
        // scramble) and never wrote back to it, so a second `COM_CHANGE_USER` after a
        // switch-requiring first one would authenticate against a nonce the client had already
        // stopped using, and legitimately fail.
        session.scramble = scramble;
        let switch = AuthSwitchRequest {
            plugin: AUTH_PLUGIN_NATIVE.into(),
            scramble,
        };
        send(stream, seq, &switch.encode())?;
        stream.flush()?;
        let (pkt_seq, switch_payload) = match read(stream, &shared.stop, auth_phase_max_len(shared))
        {
            Ok(p) => p,
            Err(e) if e.kind() == SHUTDOWN_ERROR_KIND => return Ok(false),
            Err(e) => return Err(e),
        };
        seq.continue_after(pkt_seq);
        auth_response = switch_payload;
    }

    if shared.accounts_initialized {
        match server_session.change_user(&request.username, &scramble, &auth_response) {
            Ok(()) => {}
            Err(HtapError::DurablePending { .. }) => {
                send_err(stream, seq, ER_UNKNOWN, "resource busy")?;
                return Ok(true);
            }
            Err(_) => {
                tracing::warn!(user = %request.username, "COM_CHANGE_USER access denied");
                send_err(
                    stream,
                    seq,
                    ER_ACCESS_DENIED,
                    &format!("Access denied for user '{}'", request.username),
                )?;
                return Ok(false);
            }
        }
        registry.clear();
        send(
            stream,
            seq,
            &build_command_ok(0, "", session_status_flags(server_session, false)),
        )?;
    } else {
        // `WireServer::start` bootstraps catalog accounts before listening, so this legacy
        // shared-password fallback is unreachable in normal operation.
        let authorized = verify_credentials(&scramble, shared.password.as_deref(), &auth_response);
        if !authorized {
            tracing::warn!(user = %request.username, "COM_CHANGE_USER access denied");
            send_err(
                stream,
                seq,
                ER_ACCESS_DENIED,
                &format!(
                    "Access denied for user '{}' (using password: {})",
                    request.username,
                    if auth_response.is_empty() {
                        "NO"
                    } else {
                        "YES"
                    }
                ),
            )?;
            return Ok(false);
        }

        match server_session.reset() {
            Ok(()) => {
                registry.clear();
                send(
                    stream,
                    seq,
                    &build_command_ok(0, "", session_status_flags(server_session, false)),
                )?;
            }
            Err(err) => {
                let (code, state) = map_htap_error(&err);
                send_err(stream, seq, (code, state), &err.to_string())?;
            }
        }
    }
    Ok(true)
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
    fn advertised_capabilities_include_ssl_only_with_tls_config() {
        let server = Arc::new(LocalServer::open(tempfile::tempdir().unwrap().path()).unwrap());
        let plain = Shared {
            server: Arc::clone(&server),
            password: None,
            tls_resolver: None,
            tls_server_config: None,
            tls_cert_paths: None,
            require_secure_transport: false,
            stop: Arc::new(AtomicBool::new(false)),
            read_timeout: Duration::from_millis(1),
            max_allowed_packet: 1024,
            compression_enabled: true,
            accounts_initialized: false,
            next_connection_id: AtomicU32::new(1),
        };
        assert_eq!(advertised_capabilities(&plain) & CLIENT_SSL, 0);
        assert_ne!(advertised_capabilities(&plain) & CLIENT_COMPRESS, 0);
        assert_ne!(
            advertised_capabilities(&plain) & CLIENT_ZSTD_COMPRESSION_ALGORITHM,
            0
        );

        let disabled = Shared {
            compression_enabled: false,
            ..plain
        };
        assert_eq!(advertised_capabilities(&disabled) & CLIENT_COMPRESS, 0);
        assert_eq!(
            advertised_capabilities(&disabled) & CLIENT_ZSTD_COMPRESSION_ALGORITHM,
            0
        );
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

    /// Finding 4 (fix pass): `connection_count`'s decrement used to live in the spawned closure
    /// *after* `handle_connection` returned, so a panic inside that call unwound straight past
    /// it and leaked the count forever. It now lives in [`ConnectionGuard::drop`], which Rust
    /// guarantees runs during unwinding: this test proves that guarantee holds for our guard by
    /// panicking with one alive and observing both its effects (registry removal and the count
    /// decrement) afterward, rather than exercising a whole real connection thread (there is no
    /// existing seam to make `handle_connection` itself panic on demand, and adding one purely
    /// for this test would be a bigger surface change than the guard's own `Drop` impl, which is
    /// exactly what this bug was in).
    #[test]
    fn connection_guard_decrements_count_and_unregisters_on_panic_unwind() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let dummy = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let registry: Arc<Mutex<HashMap<u32, TcpStream>>> = Arc::new(Mutex::new(HashMap::new()));
        registry.lock().insert(7, dummy);
        let count = Arc::new(AtomicUsize::new(1));

        let guard_registry = Arc::clone(&registry);
        let guard_count = Arc::clone(&count);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = ConnectionGuard {
                id: 7,
                registry: guard_registry,
                connection_count: guard_count,
            };
            panic!("simulated connection-thread panic");
        }));

        assert!(result.is_err(), "the panic must have propagated");
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "ConnectionGuard::drop must decrement connection_count even on unwind"
        );
        assert!(
            !registry.lock().contains_key(&7),
            "ConnectionGuard::drop must unregister even on unwind"
        );
    }
}
