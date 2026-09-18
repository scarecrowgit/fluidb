//! Minimal MySQL text-protocol client used by `htap-client::RemoteClient` and the tests.

use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};

use htap_common::types::{ColumnDef, Row};

use crate::binary_codec::{decode_binary_row, decode_stmt_prepare_ok, encode_execute_request};
use crate::codec::{read_lenenc_int, read_message, write_message, SeqCounter};
use crate::compression::{CompressedStream, CompressionAlgorithm};
use crate::error_map::{parse_err_payload, WireError};
use crate::handshake::{AuthSwitchRequest, HandshakeResponse41, HandshakeV10};
use crate::proto::*;
use crate::result_codec::{
    decode_text_row, is_resultset_terminator, parse_column_def41, parse_ok_payload,
    parse_terminator_status, OkPacket,
};
use crate::sha1::scramble_native_password;
use crate::tls::{perform_client_tls_handshake, Conn};
use htap_common::types::Value;

/// Compression behavior requested when connecting to a wire server.
#[derive(Debug, Clone, Default)]
pub enum CompressionMode {
    /// Do not request protocol compression.
    #[default]
    Disabled,
    /// Request MySQL zlib compression.
    Zlib,
    /// Request MySQL zstd compression at the supplied level.
    Zstd {
        /// Compression level requested from the server.
        level: u8,
    },
}

/// TLS behavior used when connecting to a wire server.
#[derive(Debug, Clone, Default)]
pub enum TlsMode {
    /// Do not request TLS.
    #[default]
    Disabled,
    /// Require TLS and validate the server certificate.
    Required {
        /// PEM bundle containing trusted certificate authorities.
        ca_cert: PathBuf,
        /// DNS name or IP address expected in the server certificate. If omitted, use the
        /// address selected for the connection.
        server_name: Option<String>,
    },
    /// Use TLS without validating the server certificate. Do not use in production.
    InsecureSkipVerifyDoNotUseInProduction,
}

#[derive(Debug)]
struct SkipCertificateVerification {
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for SkipCertificateVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn encode_ssl_request(capability_flags: u32) -> Vec<u8> {
    let mut request = Vec::with_capacity(32);
    request.extend_from_slice(&capability_flags.to_le_bytes());
    request.extend_from_slice(&(1u32 << 24).to_le_bytes());
    request.push(COLLATION_UTF8MB4 as u8);
    request.extend_from_slice(&[0u8; 23]);
    request
}

fn load_root_store(ca_cert: &PathBuf) -> io::Result<rustls::RootCertStore> {
    let file = File::open(ca_cert)?;
    let mut reader = BufReader::new(file);
    let certs = rustls_pemfile::certs(&mut reader).collect::<Result<Vec<_>, _>>()?;

    let mut roots = rustls::RootCertStore::empty();
    if roots.add_parsable_certificates(certs).0 == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "TLS CA certificate PEM contains no certificates",
        ));
    }
    Ok(roots)
}

fn tls_server_name(name: Option<&str>, peer: SocketAddr) -> io::Result<ServerName<'static>> {
    let name = name
        .map(str::to_owned)
        .unwrap_or_else(|| peer.ip().to_string());
    if let Ok(ip) = name.parse::<IpAddr>() {
        return Ok(ServerName::IpAddress(ip.into()));
    }
    ServerName::try_from(name).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid TLS server name: {e}"),
        )
    })
}

fn client_tls_config(mode: &TlsMode) -> io::Result<Arc<rustls::ClientConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid TLS protocol configuration: {e}"),
            )
        })?;
    let config = match mode {
        TlsMode::Required { ca_cert, .. } => builder
            .with_root_certificates(load_root_store(ca_cert)?)
            .with_no_client_auth(),
        TlsMode::InsecureSkipVerifyDoNotUseInProduction => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(SkipCertificateVerification { provider }))
            .with_no_client_auth(),
        TlsMode::Disabled => unreachable!("TLS configuration requested while TLS is disabled"),
    };
    Ok(Arc::new(config))
}

enum ClientStream {
    Plain(Conn),
    Compressed(CompressedStream<Conn>),
}

impl std::fmt::Debug for ClientStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Plain(_) => f.write_str("ClientStream::Plain"),
            Self::Compressed(_) => f.write_str("ClientStream::Compressed"),
        }
    }
}

impl Read for ClientStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.read(buf),
            Self::Compressed(stream) => stream.read(buf),
        }
    }
}

impl Write for ClientStream {
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

impl ClientStream {
    fn tcp(&self) -> &TcpStream {
        match self {
            Self::Plain(stream) => stream.tcp(),
            Self::Compressed(stream) => stream.get_ref().tcp(),
        }
    }

    fn reset_sequence(&mut self) {
        if let Self::Compressed(stream) = self {
            stream.reset_sequence();
        }
    }
}

/// Connection options.
#[derive(Debug, Clone)]
pub struct ClientOptions {
    /// User name authenticated by the server against catalog accounts with
    /// `mysql_native_password`. Defaults to the bootstrapped `root` account.
    pub username: String,
    /// Password for `mysql_native_password`.
    pub password: Option<String>,
    /// Initial database.
    pub database: Option<String>,
    /// Whether to request `CLIENT_DEPRECATE_EOF`.
    pub deprecate_eof: bool,
    /// Connect timeout.
    pub connect_timeout: Duration,
    /// Maximum total size, in bytes, of one reassembled protocol message this client will accept
    /// from the server (Phase 11 plan task 8); mirrors `htap_wire::WireServerConfig::
    /// max_allowed_packet`'s role on the server side, applied to outgoing messages too. Defaults
    /// to MySQL's own default, 64 MiB (`htap_sql::DEFAULT_MAX_ALLOWED_PACKET`).
    pub max_allowed_packet: usize,
    /// Whether to request `CLIENT_MULTI_STATEMENTS`/`CLIENT_MULTI_RESULTS` (Phase 11 plan task
    /// 10). When `false` (the default), a `COM_QUERY` containing more than one statement is
    /// rejected by the server exactly as before this capability existed; see
    /// [`WireClient::query_multi`].
    pub multi_statements: bool,
    /// TLS behavior for the connection.
    pub tls: TlsMode,
    /// Protocol compression behavior for the connection.
    pub compression: CompressionMode,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            username: "root".into(),
            password: None,
            database: None,
            deprecate_eof: true,
            tls: TlsMode::Disabled,
            compression: CompressionMode::Disabled,
            connect_timeout: Duration::from_secs(5),
            max_allowed_packet: htap_sql::DEFAULT_MAX_ALLOWED_PACKET as usize,
            multi_statements: false,
        }
    }
}

/// Metadata `COM_STMT_PREPARE` returned for a statement (Phase 11 plan task 5), enough to drive a
/// later `COM_STMT_EXECUTE`/`COM_STMT_CLOSE`.
#[derive(Debug, Clone, PartialEq)]
pub struct ClientPreparedStatement {
    /// Server-assigned statement id, echoed back on every `EXECUTE`/`CLOSE`.
    pub stmt_id: u32,
    /// Number of `?` placeholders.
    pub num_params: u16,
    /// Best-effort result-set column schema (`htap_sql::resolve_prepare_output_schema`'s
    /// best-effort inference on the server): empty when the statement produces no result set, or
    /// its shape could not be statically inferred (see that function's doc for exactly when).
    pub columns: Vec<ColumnDef>,
}

/// Result of a `COM_QUERY`.
#[derive(Debug, Clone, PartialEq)]
pub enum WireResult {
    /// OK packet (DDL, DML, or shim statement).
    Ok(OkPacket),
    /// Tabular result set.
    Rows {
        /// Column definitions.
        columns: Vec<ColumnDef>,
        /// Decoded rows.
        rows: Vec<Row>,
    },
}

/// Result of [`WireClient::query_multi`] (Phase 11 plan task 10).
#[derive(Debug)]
pub struct MultiQueryOutcome {
    /// One [`WireResult`] per statement that ran successfully, in order.
    pub results: Vec<WireResult>,
    /// `Some` if the batch stopped early because a statement failed (the server always makes
    /// that statement's error its final packet); `None` if every statement in the batch ran to
    /// completion.
    pub error: Option<WireError>,
}

/// A connected client.
#[derive(Debug)]
pub struct WireClient {
    stream: ClientStream,
    deprecate_eof: bool,
    server_version: String,
    connection_id: u32,
    max_allowed_packet: usize,
}

impl WireClient {
    /// Connects with default options and the given password.
    pub fn connect<A: ToSocketAddrs>(addr: A, password: Option<&str>) -> Result<Self, WireError> {
        Self::connect_with(
            addr,
            ClientOptions {
                password: password.map(str::to_string),
                ..ClientOptions::default()
            },
        )
    }

    /// Connects and authenticates with explicit options.
    pub fn connect_with<A: ToSocketAddrs>(
        addr: A,
        options: ClientOptions,
    ) -> Result<Self, WireError> {
        let mut last_err = io::Error::new(io::ErrorKind::AddrNotAvailable, "no address");
        let mut stream = None;
        for a in addr.to_socket_addrs()? {
            match TcpStream::connect_timeout(&a, options.connect_timeout) {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(e) => last_err = e,
            }
        }
        let mut tcp_stream = stream.ok_or(WireError::Io(last_err))?;
        let _ = tcp_stream.set_nodelay(true);

        let (seq0, payload) = read_message(&mut tcp_stream, options.max_allowed_packet)?;
        if payload.first() == Some(&ERR_HEADER) {
            let (code, sqlstate, message) = parse_err_payload(&payload)?;
            return Err(WireError::Server {
                code,
                sqlstate,
                message,
            });
        }
        let handshake = HandshakeV10::decode(&payload)?;
        let mut seq = SeqCounter::new();
        seq.continue_after(seq0);

        let mut caps = CLIENT_LONG_PASSWORD
            | CLIENT_PROTOCOL_41
            | CLIENT_TRANSACTIONS
            | CLIENT_SECURE_CONNECTION
            | CLIENT_PLUGIN_AUTH
            | CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA;
        if options.deprecate_eof {
            caps |= CLIENT_DEPRECATE_EOF;
        }
        if options.database.is_some() {
            caps |= CLIENT_CONNECT_WITH_DB;
        }
        if options.multi_statements {
            caps |= CLIENT_MULTI_STATEMENTS | CLIENT_MULTI_RESULTS;
        }
        match &options.compression {
            CompressionMode::Disabled => {}
            CompressionMode::Zlib => caps |= CLIENT_COMPRESS,
            CompressionMode::Zstd { .. } => caps |= CLIENT_ZSTD_COMPRESSION_ALGORITHM,
        }
        if !matches!(options.tls, TlsMode::Disabled) {
            caps |= CLIENT_SSL;
        }
        caps &= handshake.capabilities | CLIENT_CONNECT_WITH_DB;
        if caps & CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA == 0 || caps & CLIENT_PLUGIN_AUTH == 0 {
            return Err(WireError::Protocol(
                "server does not support plugin authentication".into(),
            ));
        }

        let mut stream = if matches!(options.tls, TlsMode::Disabled) {
            Conn::Plain(tcp_stream)
        } else {
            if caps & CLIENT_SSL == 0 {
                return Err(WireError::Protocol("server does not support TLS".into()));
            }
            write_message(&mut tcp_stream, &mut seq, &encode_ssl_request(caps))?;
            let peer = tcp_stream.peer_addr()?;
            let server_name = match &options.tls {
                TlsMode::Required { server_name, .. } => {
                    tls_server_name(server_name.as_deref(), peer)?
                }
                TlsMode::InsecureSkipVerifyDoNotUseInProduction => tls_server_name(None, peer)?,
                TlsMode::Disabled => unreachable!("TLS mode was checked above"),
            };
            let tls_connection =
                rustls::ClientConnection::new(client_tls_config(&options.tls)?, server_name)
                    .map_err(|e| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("invalid TLS client connection: {e}"),
                        )
                    })?;
            perform_client_tls_handshake(tcp_stream, tls_connection)?
        };

        let password = options.password.as_deref().unwrap_or("").as_bytes();
        let response = HandshakeResponse41 {
            capability_flags: caps,
            max_packet_size: 1 << 24,
            charset: COLLATION_UTF8MB4 as u8,
            username: options.username.clone(),
            auth_response: scramble_native_password(&handshake.scramble, password),
            database: options.database.clone(),
            auth_plugin: Some(AUTH_PLUGIN_NATIVE.into()),
            zstd_compression_level: match options.compression {
                CompressionMode::Zstd { level } => Some(level),
                CompressionMode::Disabled | CompressionMode::Zlib => None,
            },
        };
        write_message(&mut stream, &mut seq, &response.encode())?;

        let (s, payload) = read_message(&mut stream, options.max_allowed_packet)?;
        seq.continue_after(s);
        let payload = match payload.first() {
            Some(&EOF_HEADER) => {
                let switch = AuthSwitchRequest::decode(&payload)?;
                if switch.plugin != AUTH_PLUGIN_NATIVE {
                    return Err(WireError::Protocol(format!(
                        "unsupported auth plugin {}",
                        switch.plugin
                    )));
                }
                let resp = scramble_native_password(&switch.scramble, password);
                write_message(&mut stream, &mut seq, &resp)?;
                read_message(&mut stream, options.max_allowed_packet)?.1
            }
            _ => payload,
        };
        match payload.first() {
            Some(&OK_HEADER) => {}
            Some(&ERR_HEADER) => {
                let (code, sqlstate, message) = parse_err_payload(&payload)?;
                return Err(WireError::Server {
                    code,
                    sqlstate,
                    message,
                });
            }
            _ => return Err(WireError::Protocol("unexpected auth reply".into())),
        }

        let compression = match &options.compression {
            CompressionMode::Zlib if caps & CLIENT_COMPRESS != 0 => {
                Some(CompressionAlgorithm::Zlib)
            }
            CompressionMode::Zstd { level } if caps & CLIENT_ZSTD_COMPRESSION_ALGORITHM != 0 => {
                Some(CompressionAlgorithm::Zstd {
                    level: i32::from(*level),
                })
            }
            CompressionMode::Disabled | CompressionMode::Zlib | CompressionMode::Zstd { .. } => {
                None
            }
        };
        let stream = match compression {
            Some(compression) => ClientStream::Compressed(CompressedStream::new(
                stream,
                Some(compression),
                options.max_allowed_packet,
            )),
            None => ClientStream::Plain(stream),
        };

        Ok(Self {
            stream,
            deprecate_eof: caps & CLIENT_DEPRECATE_EOF != 0,
            server_version: handshake.server_version,
            connection_id: handshake.connection_id,
            max_allowed_packet: options.max_allowed_packet,
        })
    }

    /// Server version string from the handshake.
    pub fn server_version(&self) -> &str {
        &self.server_version
    }

    /// Connection id assigned by the server.
    pub fn connection_id(&self) -> u32 {
        self.connection_id
    }

    /// Whether `CLIENT_DEPRECATE_EOF` is in effect.
    pub fn deprecate_eof(&self) -> bool {
        self.deprecate_eof
    }

    /// Local socket address.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.stream.tcp().local_addr()
    }

    /// Sends a raw command message (split into as many physical packets as needed, Phase 11 plan
    /// task 8) and returns the first response message's payload (reassembled the same way).
    pub fn send_raw_command(&mut self, command: u8, body: &[u8]) -> Result<Vec<u8>, WireError> {
        let mut payload = Vec::with_capacity(1 + body.len());
        payload.push(command);
        payload.extend_from_slice(body);
        self.stream.reset_sequence();
        write_message(&mut self.stream, &mut SeqCounter::new(), &payload)?;
        self.stream.flush()?;
        Ok(self.read_response_message()?.1)
    }

    /// Reads one reassembled response message, enforcing this client's configured
    /// `max_allowed_packet`.
    fn read_response_message(&mut self) -> io::Result<(u8, Vec<u8>)> {
        read_message(&mut self.stream, self.max_allowed_packet)
    }

    fn expect_ok(payload: &[u8]) -> Result<OkPacket, WireError> {
        match payload.first() {
            Some(&OK_HEADER) => Ok(parse_ok_payload(payload)?),
            Some(&ERR_HEADER) => {
                let (code, sqlstate, message) = parse_err_payload(payload)?;
                Err(WireError::Server {
                    code,
                    sqlstate,
                    message,
                })
            }
            _ => Err(WireError::Protocol("expected OK or ERR packet".into())),
        }
    }

    /// `COM_PING`.
    pub fn ping(&mut self) -> Result<(), WireError> {
        let payload = self.send_raw_command(COM_PING, &[])?;
        Self::expect_ok(&payload).map(|_| ())
    }

    /// `COM_INIT_DB`.
    pub fn init_db(&mut self, name: &str) -> Result<(), WireError> {
        let payload = self.send_raw_command(COM_INIT_DB, name.as_bytes())?;
        Self::expect_ok(&payload).map(|_| ())
    }

    /// `COM_QUIT`; closes the connection.
    pub fn quit(mut self) -> Result<(), WireError> {
        let mut payload = vec![COM_QUIT];
        payload.shrink_to_fit();
        write_message(&mut self.stream, &mut SeqCounter::new(), &payload)?;
        self.stream.flush()?;
        Ok(())
    }

    /// Executes one statement with `COM_QUERY`.
    ///
    /// If the connection negotiated `CLIENT_MULTI_STATEMENTS` and `sql` contains more than one
    /// statement, this only returns the *first* result; use [`Self::query_multi`] instead to
    /// read the whole batch.
    pub fn query(&mut self, sql: &str) -> Result<WireResult, WireError> {
        let first = self.send_raw_command(COM_QUERY, sql.as_bytes())?;
        self.read_resultset(&first, decode_text_row)
    }

    /// Executes `sql` with `COM_QUERY` and reads every result in the response, following
    /// `SERVER_MORE_RESULTS_EXISTS` until it is unset (Phase 11 plan task 10,
    /// `CLIENT_MULTI_STATEMENTS`; see [`ClientOptions::multi_statements`]). Works identically for
    /// a single-statement `sql`, returning exactly one result.
    ///
    /// A server error partway through the batch (the server always stops the batch at its first
    /// error) ends up in [`MultiQueryOutcome::error`], alongside every result already read for
    /// statements that ran before it; this is not a transport failure, so it is not an `Err`
    /// here. Only a genuine transport/protocol failure (a malformed packet, a dropped
    /// connection, ...) returns `Err`.
    pub fn query_multi(&mut self, sql: &str) -> Result<MultiQueryOutcome, WireError> {
        let mut first = self.send_raw_command(COM_QUERY, sql.as_bytes())?;
        let mut results = Vec::new();
        loop {
            match first.first() {
                Some(&ERR_HEADER) => {
                    let (code, sqlstate, message) = parse_err_payload(&first)?;
                    return Ok(MultiQueryOutcome {
                        results,
                        error: Some(WireError::Server {
                            code,
                            sqlstate,
                            message,
                        }),
                    });
                }
                Some(&OK_HEADER) => {
                    let ok = parse_ok_payload(&first)?;
                    let more_results = ok.status & SERVER_MORE_RESULTS_EXISTS != 0;
                    results.push(WireResult::Ok(ok));
                    if !more_results {
                        break;
                    }
                    first = self.read_response_message()?.1;
                }
                Some(&NULL_MARKER) => {
                    return Err(WireError::Protocol(
                        "LOCAL INFILE requests are not supported".into(),
                    ))
                }
                Some(_) => {
                    let (columns, rows, status) =
                        self.read_columns_and_rows(&first, decode_text_row)?;
                    let more_results = status & SERVER_MORE_RESULTS_EXISTS != 0;
                    results.push(WireResult::Rows { columns, rows });
                    if !more_results {
                        break;
                    }
                    first = self.read_response_message()?.1;
                }
                None => return Err(WireError::Protocol("empty response packet".into())),
            }
        }
        Ok(MultiQueryOutcome {
            results,
            error: None,
        })
    }

    /// Reads the rest of a result-set response (or an OK/ERR already fully contained in `first`),
    /// given the first response message's already-read payload and a row decoder — shared by the
    /// text protocol ([`Self::query`], via [`decode_text_row`]) and the binary protocol
    /// ([`Self::execute_prepared`], via [`decode_binary_row`]); the two differ only in how a row's
    /// bytes decode, never in message sequencing.
    fn read_resultset(
        &mut self,
        first: &[u8],
        decode_row: impl Fn(&[u8], &[ColumnDef]) -> io::Result<Row>,
    ) -> Result<WireResult, WireError> {
        match first.first() {
            Some(&OK_HEADER) | Some(&ERR_HEADER) => Self::expect_ok(first).map(WireResult::Ok),
            Some(&NULL_MARKER) => Err(WireError::Protocol(
                "LOCAL INFILE requests are not supported".into(),
            )),
            Some(_) => {
                let (columns, rows, _status) = self.read_columns_and_rows(first, decode_row)?;
                Ok(WireResult::Rows { columns, rows })
            }
            None => Err(WireError::Protocol("empty response packet".into())),
        }
    }

    /// Reads a whole result set's column definitions and rows given the already-read first
    /// message (the column count), returning the terminator's status-flags word alongside them
    /// (Phase 11 plan task 10: used by [`Self::query_multi`] to detect
    /// `SERVER_MORE_RESULTS_EXISTS`; [`Self::read_resultset`] just discards it).
    fn read_columns_and_rows(
        &mut self,
        first: &[u8],
        decode_row: impl Fn(&[u8], &[ColumnDef]) -> io::Result<Row>,
    ) -> Result<(Vec<ColumnDef>, Vec<Row>, u16), WireError> {
        let mut pos = 0;
        let column_count = read_lenenc_int(first, &mut pos)? as usize;
        let mut columns = Vec::with_capacity(column_count);
        for _ in 0..column_count {
            let (_, payload) = self.read_response_message()?;
            columns.push(parse_column_def41(&payload)?);
        }
        if !self.deprecate_eof {
            let (_, eof) = self.read_response_message()?;
            if !is_resultset_terminator(&eof) {
                return Err(WireError::Protocol(
                    "expected EOF after column definitions".into(),
                ));
            }
        }
        let mut rows = Vec::new();
        loop {
            let (_, payload) = self.read_response_message()?;
            if is_resultset_terminator(&payload) {
                let status = parse_terminator_status(&payload, self.deprecate_eof)?;
                return Ok((columns, rows, status));
            }
            if payload.first() == Some(&ERR_HEADER) {
                let (code, sqlstate, message) = parse_err_payload(&payload)?;
                return Err(WireError::Server {
                    code,
                    sqlstate,
                    message,
                });
            }
            rows.push(decode_row(&payload, &columns)?);
        }
    }

    /// `COM_STMT_PREPARE` (Phase 11 plan task 5).
    ///
    /// # Errors
    ///
    /// Returns [`WireError::Server`] if the server rejects the statement (e.g. an unsupported
    /// statement shape, or a placeholder in an unsupported position).
    pub fn prepare(&mut self, sql: &str) -> Result<ClientPreparedStatement, WireError> {
        let mut body = Vec::with_capacity(sql.len());
        body.extend_from_slice(sql.as_bytes());
        let first = self.send_raw_command(COM_STMT_PREPARE, &body)?;
        if first.first() == Some(&ERR_HEADER) {
            let (code, sqlstate, message) = parse_err_payload(&first)?;
            return Err(WireError::Server {
                code,
                sqlstate,
                message,
            });
        }
        let ok = decode_stmt_prepare_ok(&first)?;

        // Parameter definitions block: `ok.num_params` generic column defs, then one terminator,
        // present iff `num_params > 0` (`server::respond_stmt_prepare`'s response shape).
        if ok.num_params > 0 {
            for _ in 0..ok.num_params {
                self.read_response_message()?;
            }
            self.read_response_message()?;
        }

        // Result-set column block: `ok.num_columns` column defs, then one terminator, present iff
        // `num_columns > 0` (best-effort inference; 0 when the statement produces no result set
        // or its shape could not be statically inferred).
        let mut columns = Vec::with_capacity(ok.num_columns as usize);
        if ok.num_columns > 0 {
            for _ in 0..ok.num_columns {
                let (_, payload) = self.read_response_message()?;
                columns.push(parse_column_def41(&payload)?);
            }
            self.read_response_message()?;
        }

        Ok(ClientPreparedStatement {
            stmt_id: ok.stmt_id,
            num_params: ok.num_params,
            columns,
        })
    }

    /// `COM_STMT_EXECUTE` (Phase 11 plan task 5): always sends `new_params_bound_flag = 1` with a
    /// fresh type for every parameter (see [`encode_execute_request`]'s doc comment on the
    /// type-per-`Value` mapping).
    ///
    /// # Errors
    ///
    /// Returns [`WireError::Server`] if the server rejects the execution (e.g. `stmt.stmt_id`
    /// refers to an unknown/closed statement, or `params.len()` does not match the statement's
    /// own parameter count).
    pub fn execute_prepared(
        &mut self,
        stmt: &ClientPreparedStatement,
        params: &[Value],
    ) -> Result<WireResult, WireError> {
        let body = encode_execute_request(stmt.stmt_id, params)?;
        let first = self.send_raw_command(COM_STMT_EXECUTE, &body)?;
        self.read_resultset(&first, decode_binary_row)
    }

    /// `COM_STMT_CLOSE`; no response by protocol, so this never fails on the server's account
    /// (only a transport failure sending the request itself returns an error). A later
    /// `execute_prepared`/`close_stmt` referencing the same `stmt_id` gets `ER_UNKNOWN_STMT_HANDLER`
    /// from the server exactly like any other unknown statement id.
    pub fn close_stmt(&mut self, stmt_id: u32) -> Result<(), WireError> {
        let mut body = Vec::with_capacity(4);
        body.extend_from_slice(&stmt_id.to_le_bytes());
        let mut payload = Vec::with_capacity(1 + body.len());
        payload.push(COM_STMT_CLOSE);
        payload.extend_from_slice(&body);
        self.stream.reset_sequence();
        write_message(&mut self.stream, &mut SeqCounter::new(), &payload)?;
        self.stream.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssl_request_has_the_required_layout() {
        let caps = CLIENT_PROTOCOL_41 | CLIENT_SSL;
        let request = encode_ssl_request(caps);
        assert_eq!(request.len(), 32);
        assert_eq!(
            u32::from_le_bytes(request[..4].try_into().unwrap()) & CLIENT_SSL,
            CLIENT_SSL
        );
    }
}
