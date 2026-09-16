//! Connection-phase packets: initial handshake, client response, and auth switch.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::codec::{
    read_fixed, read_lenenc_str, read_null_terminated, read_u16, read_u32, write_lenenc_str,
    write_null_terminated,
};
use crate::proto::*;

/// Length of the authentication scramble.
pub const SCRAMBLE_LEN: usize = 20;

static SCRAMBLE_COUNTER: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);

/// Generates a 20-byte scramble of printable, non-zero ASCII bytes.
///
/// The generator is a xorshift64* stream seeded from the wall clock and a process-wide
/// counter. It is **not** cryptographically secure; it only needs to make replays of a
/// captured native-password response unlikely across connections. The wire layer offers no
/// TLS and is intended for loopback deployments (see the security contract in the docs).
pub fn generate_scramble() -> [u8; SCRAMBLE_LEN] {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let counter = SCRAMBLE_COUNTER.fetch_add(0x2545_F491_4F6C_DD1D, Ordering::Relaxed);
    let mut state = nanos ^ counter.rotate_left(17) ^ 0xD1B5_4A32_D192_ED03;
    if state == 0 {
        state = 0x1234_5678_9ABC_DEF1;
    }
    let mut out = [0u8; SCRAMBLE_LEN];
    for b in out.iter_mut() {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let v = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
        // Printable ASCII range 0x21..=0x7e, never zero (the first 8 bytes precede a NUL filler).
        *b = 0x21 + ((v >> 56) % 94) as u8;
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scramble_is_printable_and_varies() {
        let a = generate_scramble();
        let b = generate_scramble();
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
}
