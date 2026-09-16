//! Minimal MySQL text-protocol client used by `htap-client::RemoteClient` and the tests.

use std::io::{self, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use htap_common::types::{ColumnDef, Row};

use crate::codec::{read_lenenc_int, read_packet, write_packet, SeqCounter};
use crate::error_map::{parse_err_payload, WireError};
use crate::handshake::{AuthSwitchRequest, HandshakeResponse41, HandshakeV10};
use crate::proto::*;
use crate::result_codec::{
    decode_text_row, is_resultset_terminator, parse_column_def41, parse_ok_payload, OkPacket,
};
use crate::sha1::scramble_native_password;

/// Connection options.
#[derive(Debug, Clone)]
pub struct ClientOptions {
    /// User name sent in the handshake (not verified by the server).
    pub username: String,
    /// Password for `mysql_native_password`.
    pub password: Option<String>,
    /// Initial database.
    pub database: Option<String>,
    /// Whether to request `CLIENT_DEPRECATE_EOF`.
    pub deprecate_eof: bool,
    /// Connect timeout.
    pub connect_timeout: Duration,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            username: "root".into(),
            password: None,
            database: None,
            deprecate_eof: true,
            connect_timeout: Duration::from_secs(5),
        }
    }
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

/// A connected client.
#[derive(Debug)]
pub struct WireClient {
    stream: TcpStream,
    deprecate_eof: bool,
    server_version: String,
    connection_id: u32,
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
        let mut stream = stream.ok_or(WireError::Io(last_err))?;
        let _ = stream.set_nodelay(true);

        let (seq0, payload) = read_packet(&mut stream)?;
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
        caps &= handshake.capabilities | CLIENT_CONNECT_WITH_DB;
        if caps & CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA == 0 || caps & CLIENT_PLUGIN_AUTH == 0 {
            return Err(WireError::Protocol(
                "server does not support plugin authentication".into(),
            ));
        }
        let password = options.password.as_deref().unwrap_or("").as_bytes();
        let response = HandshakeResponse41 {
            capability_flags: caps,
            max_packet_size: 1 << 24,
            charset: COLLATION_UTF8MB4 as u8,
            username: options.username.clone(),
            auth_response: scramble_native_password(&handshake.scramble, password),
            database: options.database.clone(),
            auth_plugin: Some(AUTH_PLUGIN_NATIVE.into()),
        };
        write_packet(&mut stream, seq.advance(), &response.encode())?;

        let (s, payload) = read_packet(&mut stream)?;
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
                write_packet(&mut stream, seq.advance(), &resp)?;
                read_packet(&mut stream)?.1
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

        Ok(Self {
            stream,
            deprecate_eof: caps & CLIENT_DEPRECATE_EOF != 0,
            server_version: handshake.server_version,
            connection_id: handshake.connection_id,
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
        self.stream.local_addr()
    }

    /// Sends a raw command packet and returns the first response packet's payload.
    pub fn send_raw_command(&mut self, command: u8, body: &[u8]) -> Result<Vec<u8>, WireError> {
        let mut payload = Vec::with_capacity(1 + body.len());
        payload.push(command);
        payload.extend_from_slice(body);
        write_packet(&mut self.stream, 0, &payload)?;
        Ok(read_packet(&mut self.stream)?.1)
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
        write_packet(&mut self.stream, 0, &payload)?;
        self.stream.flush()?;
        Ok(())
    }

    /// Executes one statement with `COM_QUERY`.
    pub fn query(&mut self, sql: &str) -> Result<WireResult, WireError> {
        let first = self.send_raw_command(COM_QUERY, sql.as_bytes())?;
        match first.first() {
            Some(&OK_HEADER) | Some(&ERR_HEADER) => Self::expect_ok(&first).map(WireResult::Ok),
            Some(&NULL_MARKER) => Err(WireError::Protocol(
                "LOCAL INFILE requests are not supported".into(),
            )),
            Some(_) => {
                let mut pos = 0;
                let column_count = read_lenenc_int(&first, &mut pos)? as usize;
                let mut columns = Vec::with_capacity(column_count);
                for _ in 0..column_count {
                    let (_, payload) = read_packet(&mut self.stream)?;
                    columns.push(parse_column_def41(&payload)?);
                }
                if !self.deprecate_eof {
                    let (_, eof) = read_packet(&mut self.stream)?;
                    if !is_resultset_terminator(&eof) {
                        return Err(WireError::Protocol(
                            "expected EOF after column definitions".into(),
                        ));
                    }
                }
                let mut rows = Vec::new();
                loop {
                    let (_, payload) = read_packet(&mut self.stream)?;
                    if is_resultset_terminator(&payload) {
                        break;
                    }
                    if payload.first() == Some(&ERR_HEADER) {
                        let (code, sqlstate, message) = parse_err_payload(&payload)?;
                        return Err(WireError::Server {
                            code,
                            sqlstate,
                            message,
                        });
                    }
                    rows.push(decode_text_row(&payload, &columns)?);
                }
                Ok(WireResult::Rows { columns, rows })
            }
            None => Err(WireError::Protocol("empty response packet".into())),
        }
    }
}
