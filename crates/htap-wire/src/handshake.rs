//! Connection-phase packets: initial handshake, client response, and auth switch.

use std::io;

use crate::codec::{
    read_fixed, read_lenenc_str, read_null_terminated, read_u16, read_u32, write_lenenc_str,
    write_null_terminated,
};
use crate::proto::*;

/// Length of the authentication scramble.
pub const SCRAMBLE_LEN: usize = 20;

/// Generates a 20-byte scramble of printable, non-zero ASCII bytes.
///
/// Bytes come from the OS CSPRNG via [`getrandom::fill`] (Phase 11 plan task 9), each mapped
/// into the printable, non-zero ASCII range `0x21..=0x7e` by taking it modulo 94 and offsetting
/// — this preserves enough entropy per byte (94 outcomes) to make replaying a captured
/// native-password response infeasible, while satisfying the handshake packet's requirement
/// that the scramble never contain a NUL byte (`HandshakeV10::encode`'s first 8 bytes precede a
/// NUL filler, and `AuthSwitchRequest::encode` NUL-terminates the whole 20 bytes).
///
/// # Errors
///
/// Returns the underlying [`getrandom::Error`] (wrapped as [`io::Error`]) if the OS RNG is
/// unavailable. There is no fallback: a handshake this fails just fails, rather than silently
/// falling back to a weaker generator.
pub fn generate_scramble() -> io::Result<[u8; SCRAMBLE_LEN]> {
    let mut raw = [0u8; SCRAMBLE_LEN];
    getrandom::fill(&mut raw).map_err(|e| io::Error::other(format!("getrandom failed: {e}")))?;
    let mut out = [0u8; SCRAMBLE_LEN];
    for (o, b) in out.iter_mut().zip(raw.iter()) {
        *o = 0x21 + (*b % 94);
    }
    Ok(out)
}

/// Initial handshake packet (protocol version 10) sent by the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandshakeV10 {
    /// Connection id assigned by the server.
    pub connection_id: u32,
    /// Authentication scramble.
    pub scramble: [u8; SCRAMBLE_LEN],
    /// Server capability flags.
    pub capabilities: u32,
    /// Server version string.
    pub server_version: String,
    /// Authentication plugin name.
    pub auth_plugin: String,
}

impl HandshakeV10 {
    /// Builds the server's handshake for a new connection.
    pub fn new(connection_id: u32, scramble: [u8; SCRAMBLE_LEN]) -> Self {
        Self {
            connection_id,
            scramble,
            capabilities: SERVER_CAPABILITIES,
            server_version: SERVER_VERSION.to_string(),
            auth_plugin: AUTH_PLUGIN_NATIVE.to_string(),
        }
    }

    /// Encodes the packet payload.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(128);
        buf.push(PROTOCOL_VERSION);
        write_null_terminated(&mut buf, self.server_version.as_bytes());
        buf.extend_from_slice(&self.connection_id.to_le_bytes());
        buf.extend_from_slice(&self.scramble[..8]);
        buf.push(0);
        buf.extend_from_slice(&((self.capabilities & 0xffff) as u16).to_le_bytes());
        buf.push(COLLATION_UTF8MB4 as u8);
        // Checked as part of the Phase 11 fix pass (finding 8): no `htap_server::Session` exists
        // yet at handshake time (`crate::server::authenticate` creates one only after this
        // packet and the auth exchange complete), and a session's default state is always
        // autocommit-on with no open transaction, so this literal status is exactly right here
        // — unlike the OK/EOF/terminator builders used once a session exists, which now report
        // its real `autocommit`/`in_transaction` state instead of hardcoding this same value.
        buf.extend_from_slice(&SERVER_STATUS_AUTOCOMMIT.to_le_bytes());
        buf.extend_from_slice(&((self.capabilities >> 16) as u16).to_le_bytes());
        buf.push((SCRAMBLE_LEN + 1) as u8);
        buf.extend_from_slice(&[0u8; 10]);
        buf.extend_from_slice(&self.scramble[8..]);
        buf.push(0);
        write_null_terminated(&mut buf, self.auth_plugin.as_bytes());
        buf
    }

    /// Decodes a handshake payload (client side).
    pub fn decode(payload: &[u8]) -> io::Result<Self> {
        let mut pos = 0;
        let version = read_fixed(payload, &mut pos, 1)?[0];
        if version != PROTOCOL_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported handshake protocol version {version}"),
            ));
        }
        let server_version =
            String::from_utf8_lossy(read_null_terminated(payload, &mut pos)?).into_owned();
        let connection_id = read_u32(payload, &mut pos)?;
        let part1 = read_fixed(payload, &mut pos, 8)?.to_vec();
        let _filler = read_fixed(payload, &mut pos, 1)?;
        let caps_low = read_u16(payload, &mut pos)? as u32;
        let _charset = read_fixed(payload, &mut pos, 1)?;
        let _status = read_u16(payload, &mut pos)?;
        let caps_high = read_u16(payload, &mut pos)? as u32;
        let capabilities = caps_low | (caps_high << 16);
        let auth_len = read_fixed(payload, &mut pos, 1)?[0] as usize;
        let _reserved = read_fixed(payload, &mut pos, 10)?;
        let mut scramble = [0u8; SCRAMBLE_LEN];
        scramble[..8].copy_from_slice(&part1);
        if capabilities & CLIENT_SECURE_CONNECTION != 0 {
            let part2_len = std::cmp::max(13, auth_len.saturating_sub(8));
            let part2 = read_fixed(payload, &mut pos, part2_len)?;
            scramble[8..].copy_from_slice(&part2[..12]);
        }
        let auth_plugin = if capabilities & CLIENT_PLUGIN_AUTH != 0 {
            String::from_utf8_lossy(read_null_terminated(payload, &mut pos)?).into_owned()
        } else {
            AUTH_PLUGIN_NATIVE.to_string()
        };
        Ok(Self {
            connection_id,
            scramble,
            capabilities,
            server_version,
            auth_plugin,
        })
    }
}

/// Client handshake response (protocol 4.1 layout).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandshakeResponse41 {
    /// Capability flags requested by the client.
    pub capability_flags: u32,
    /// Maximum packet size the client accepts.
    pub max_packet_size: u32,
    /// Character set id.
    pub charset: u8,
    /// User name (accepted but not verified by the server).
    pub username: String,
    /// Authentication response bytes.
    pub auth_response: Vec<u8>,
    /// Initial database, if requested.
    pub database: Option<String>,
    /// Authentication plugin proposed by the client, if any.
    pub auth_plugin: Option<String>,
}

impl HandshakeResponse41 {
    /// Returns the capability flags from a response payload without parsing the rest.
    pub fn peek_capabilities(payload: &[u8]) -> Option<u32> {
        let b = payload.get(0..4)?;
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Decodes a client response (server side).
    pub fn decode(payload: &[u8]) -> io::Result<Self> {
        let mut pos = 0;
        let capability_flags = read_u32(payload, &mut pos)?;
        if capability_flags & CLIENT_PROTOCOL_41 == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "client does not support protocol 4.1",
            ));
        }
        let max_packet_size = read_u32(payload, &mut pos)?;
        let charset = read_fixed(payload, &mut pos, 1)?[0];
        let _reserved = read_fixed(payload, &mut pos, 23)?;
        let username =
            String::from_utf8_lossy(read_null_terminated(payload, &mut pos)?).into_owned();
        let auth_response = if capability_flags & CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA != 0 {
            read_lenenc_str(payload, &mut pos)?.to_vec()
        } else if capability_flags & CLIENT_SECURE_CONNECTION != 0 {
            let len = read_fixed(payload, &mut pos, 1)?[0] as usize;
            read_fixed(payload, &mut pos, len)?.to_vec()
        } else {
            read_null_terminated(payload, &mut pos)?.to_vec()
        };
        let database = if capability_flags & CLIENT_CONNECT_WITH_DB != 0 {
            Some(String::from_utf8_lossy(read_null_terminated(payload, &mut pos)?).into_owned())
        } else {
            None
        };
        let auth_plugin = if capability_flags & CLIENT_PLUGIN_AUTH != 0 {
            // Some clients omit the plugin name when it is empty; tolerate a missing field.
            if pos < payload.len() {
                Some(String::from_utf8_lossy(read_null_terminated(payload, &mut pos)?).into_owned())
            } else {
                None
            }
        } else {
            None
        };
        // Connection attributes (CLIENT_CONNECT_ATTRS), if present, are ignored.
        Ok(Self {
            capability_flags,
            max_packet_size,
            charset,
            username,
            auth_response,
            database,
            auth_plugin,
        })
    }

    /// Encodes the response (client side). The client always uses the length-encoded
    /// auth-response layout, so `capability_flags` must include
    /// `CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA`.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(96);
        buf.extend_from_slice(&self.capability_flags.to_le_bytes());
        buf.extend_from_slice(&self.max_packet_size.to_le_bytes());
        buf.push(self.charset);
        buf.extend_from_slice(&[0u8; 23]);
        write_null_terminated(&mut buf, self.username.as_bytes());
        write_lenenc_str(&mut buf, &self.auth_response);
        if self.capability_flags & CLIENT_CONNECT_WITH_DB != 0 {
            write_null_terminated(&mut buf, self.database.as_deref().unwrap_or("").as_bytes());
        }
        if self.capability_flags & CLIENT_PLUGIN_AUTH != 0 {
            write_null_terminated(
                &mut buf,
                self.auth_plugin.as_deref().unwrap_or("").as_bytes(),
            );
        }
        buf
    }
}

/// Server request to switch the client to another authentication plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthSwitchRequest {
    /// Plugin the client must use.
    pub plugin: String,
    /// Fresh scramble for the plugin.
    pub scramble: [u8; SCRAMBLE_LEN],
}

impl AuthSwitchRequest {
    /// Encodes the packet payload.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(48);
        buf.push(EOF_HEADER);
        write_null_terminated(&mut buf, self.plugin.as_bytes());
        buf.extend_from_slice(&self.scramble);
        buf.push(0);
        buf
    }

    /// Decodes the packet payload (client side).
    pub fn decode(payload: &[u8]) -> io::Result<Self> {
        let mut pos = 0;
        let header = read_fixed(payload, &mut pos, 1)?[0];
        if header != EOF_HEADER {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not an auth switch request",
            ));
        }
        let plugin = String::from_utf8_lossy(read_null_terminated(payload, &mut pos)?).into_owned();
        let mut data = payload[pos..].to_vec();
        if data.len() == SCRAMBLE_LEN + 1 && data[SCRAMBLE_LEN] == 0 {
            data.truncate(SCRAMBLE_LEN);
        }
        if data.len() != SCRAMBLE_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("auth switch scramble has {} bytes", data.len()),
            ));
        }
        let mut scramble = [0u8; SCRAMBLE_LEN];
        scramble.copy_from_slice(&data);
        Ok(Self { plugin, scramble })
    }
}

/// Decoded `COM_CHANGE_USER` request body (Phase 11 plan task 7), after the leading command byte
/// has already been stripped by the caller.
///
/// Layout per the MySQL protocol: a NUL-terminated username; an auth response whose length
/// encoding depends on the *connection's* negotiated capability flags (lenenc if
/// `CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA`, else a 1-byte length prefix if
/// `CLIENT_SECURE_CONNECTION`, else NUL-terminated — the same three-way choice
/// [`HandshakeResponse41::decode`] makes for the initial handshake); a NUL-terminated database
/// name (possibly empty); then an optional trailer of a 2-byte character set, a NUL-terminated
/// auth plugin name (only if `CLIENT_PLUGIN_AUTH`), and connect attributes. The trailer is parsed
/// leniently (each piece only if further bytes remain) and connect attributes are always ignored
/// without requiring every trailing byte to be consumed: at least one real client
/// (`mysql_common`'s `ComChangeUserMoreData::serialize`) always writes a connect-attributes block
/// on the wire regardless of what it actually negotiated ("We'll always act like
/// CLIENT_CONNECT_ATTRS is set, this is to avoid looking into the actual connection flags",
/// `mysql_common-0.37.3/src/packets/mod.rs` line ~2199), so a decoder that demanded the trailer's
/// bytes balance exactly would reject a real driver's request whenever `CLIENT_CONNECT_ATTRS` was
/// not itself negotiated at the initial handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeUserRequest {
    /// User name (accepted but not verified beyond the password challenge/response).
    pub username: String,
    /// Authentication response bytes.
    pub auth_response: Vec<u8>,
    /// Requested database, if any (empty string and absent are both `None`).
    pub database: Option<String>,
    /// Authentication plugin proposed by the client, if any.
    pub auth_plugin: Option<String>,
}

impl ChangeUserRequest {
    /// Decodes a `COM_CHANGE_USER` request body using `capability_flags` negotiated at this
    /// connection's initial handshake (never re-negotiated by `COM_CHANGE_USER` itself).
    pub fn decode(payload: &[u8], capability_flags: u32) -> io::Result<Self> {
        let mut pos = 0;
        let username =
            String::from_utf8_lossy(read_null_terminated(payload, &mut pos)?).into_owned();
        let auth_response = if capability_flags & CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA != 0 {
            read_lenenc_str(payload, &mut pos)?.to_vec()
        } else if capability_flags & CLIENT_SECURE_CONNECTION != 0 {
            let len = read_fixed(payload, &mut pos, 1)?[0] as usize;
            read_fixed(payload, &mut pos, len)?.to_vec()
        } else {
            read_null_terminated(payload, &mut pos)?.to_vec()
        };
        let database = {
            let raw = read_null_terminated(payload, &mut pos)?;
            if raw.is_empty() {
                None
            } else {
                Some(String::from_utf8_lossy(raw).into_owned())
            }
        };
        if pos + 2 <= payload.len() {
            let _charset = read_fixed(payload, &mut pos, 2)?;
        }
        let auth_plugin = if capability_flags & CLIENT_PLUGIN_AUTH != 0 && pos < payload.len() {
            Some(String::from_utf8_lossy(read_null_terminated(payload, &mut pos)?).into_owned())
        } else {
            None
        };
        // Connect attributes, if present, are read into nothing: this decoder does not require
        // every trailing byte to be consumed (see the doc comment above).
        Ok(Self {
            username,
            auth_response,
            database,
            auth_plugin,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scramble_is_printable_and_varies() {
        let a = generate_scramble().unwrap();
        let b = generate_scramble().unwrap();
        assert!(a.iter().all(|&c| (0x21..=0x7e).contains(&c)));
        assert_ne!(a, b);
    }

    #[test]
    fn handshake_v10_round_trip() {
        let hs = HandshakeV10::new(7, *b"abcdefghijklmnopqrst");
        let bytes = hs.encode();
        assert_eq!(bytes[0], PROTOCOL_VERSION);
        let decoded = HandshakeV10::decode(&bytes).unwrap();
        assert_eq!(decoded, hs);
        assert!(HandshakeV10::decode(&bytes[..20]).is_err());
    }

    #[test]
    fn handshake_response41_round_trip_with_and_without_db() {
        let base = HandshakeResponse41 {
            capability_flags: SERVER_CAPABILITIES,
            max_packet_size: 1 << 24,
            charset: 45,
            username: "root".into(),
            auth_response: vec![1, 2, 3],
            database: None,
            auth_plugin: Some(AUTH_PLUGIN_NATIVE.into()),
        };
        let bytes = base.encode();
        assert_eq!(
            HandshakeResponse41::peek_capabilities(&bytes),
            Some(SERVER_CAPABILITIES)
        );
        let decoded = HandshakeResponse41::decode(&bytes).unwrap();
        // CONNECT_WITH_DB was set but no database given: encodes as empty string.
        assert_eq!(decoded.database.as_deref(), Some(""));
        assert_eq!(decoded.username, "root");
        assert_eq!(decoded.auth_response, vec![1, 2, 3]);
        assert_eq!(decoded.auth_plugin.as_deref(), Some(AUTH_PLUGIN_NATIVE));

        let with_db = HandshakeResponse41 {
            database: Some("htap".into()),
            capability_flags: SERVER_CAPABILITIES & !CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA
                | CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA,
            ..base.clone()
        };
        let decoded = HandshakeResponse41::decode(&with_db.encode()).unwrap();
        assert_eq!(decoded.database.as_deref(), Some("htap"));

        // Secure-connection (1-byte length) layout without lenenc flag.
        let mut secure = Vec::new();
        let caps = CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION;
        secure.extend_from_slice(&caps.to_le_bytes());
        secure.extend_from_slice(&(1u32 << 24).to_le_bytes());
        secure.push(45);
        secure.extend_from_slice(&[0u8; 23]);
        secure.extend_from_slice(b"u\0");
        secure.push(2);
        secure.extend_from_slice(&[9, 9]);
        let decoded = HandshakeResponse41::decode(&secure).unwrap();
        assert_eq!(decoded.username, "u");
        assert_eq!(decoded.auth_response, vec![9, 9]);
        assert_eq!(decoded.database, None);
        assert_eq!(decoded.auth_plugin, None);

        // Truncated payload never panics.
        assert!(HandshakeResponse41::decode(&secure[..30]).is_err());
        // Pre-4.1 clients rejected.
        let mut old = secure.clone();
        old[..4].copy_from_slice(&0u32.to_le_bytes());
        assert!(HandshakeResponse41::decode(&old).is_err());
    }

    #[test]
    fn auth_switch_round_trip() {
        let req = AuthSwitchRequest {
            plugin: AUTH_PLUGIN_NATIVE.into(),
            scramble: *b"01234567890123456789",
        };
        let bytes = req.encode();
        assert_eq!(bytes[0], EOF_HEADER);
        assert_eq!(AuthSwitchRequest::decode(&bytes).unwrap(), req);
        assert!(AuthSwitchRequest::decode(&bytes[..10]).is_err());
    }

    #[test]
    fn change_user_request_decodes_secure_connection_layout_with_trailer() {
        let caps = CLIENT_SECURE_CONNECTION | CLIENT_PLUGIN_AUTH | CLIENT_CONNECT_ATTRS;
        let mut payload = Vec::new();
        write_null_terminated(&mut payload, b"root");
        payload.push(3); // auth response length (1-byte form)
        payload.extend_from_slice(&[1, 2, 3]);
        write_null_terminated(&mut payload, b"htap");
        payload.extend_from_slice(&COLLATION_UTF8MB4.to_le_bytes());
        write_null_terminated(&mut payload, AUTH_PLUGIN_NATIVE.as_bytes());
        // A trailing connect-attributes block that this decoder never has to fully consume.
        payload.push(0); // lenenc length 0: no attributes

        let decoded = ChangeUserRequest::decode(&payload, caps).unwrap();
        assert_eq!(decoded.username, "root");
        assert_eq!(decoded.auth_response, vec![1, 2, 3]);
        assert_eq!(decoded.database.as_deref(), Some("htap"));
        assert_eq!(decoded.auth_plugin.as_deref(), Some(AUTH_PLUGIN_NATIVE));
    }

    #[test]
    fn change_user_request_decodes_minimal_payload_without_trailer() {
        let caps = CLIENT_SECURE_CONNECTION;
        let mut payload = Vec::new();
        write_null_terminated(&mut payload, b"root");
        payload.push(0); // empty auth response
        write_null_terminated(&mut payload, b""); // no database
        let decoded = ChangeUserRequest::decode(&payload, caps).unwrap();
        assert_eq!(decoded.username, "root");
        assert!(decoded.auth_response.is_empty());
        assert_eq!(decoded.database, None);
        assert_eq!(decoded.auth_plugin, None);
    }

    #[test]
    fn change_user_request_lenenc_auth_response_layout() {
        let caps = CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA;
        let mut payload = Vec::new();
        write_null_terminated(&mut payload, b"root");
        write_lenenc_str(&mut payload, &[9, 9, 9, 9]);
        write_null_terminated(&mut payload, b"");
        let decoded = ChangeUserRequest::decode(&payload, caps).unwrap();
        assert_eq!(decoded.auth_response, vec![9, 9, 9, 9]);
    }
}
