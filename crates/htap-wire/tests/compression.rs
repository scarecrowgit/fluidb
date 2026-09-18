//! End-to-end compression tests for `WireServer`.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use flate2::write::ZlibEncoder;
use flate2::Compression;
use htap_common::types::{Row, Value};
use htap_server::LocalServer;
use htap_wire::codec::{read_packet, write_packet};
use htap_wire::compression::{CompressedStream, CompressionAlgorithm};
use htap_wire::handshake::{HandshakeResponse41, HandshakeV10};
use htap_wire::proto::*;
use htap_wire::sha1::scramble_native_password;
use htap_wire::{
    ClientOptions, CompressionMode, TlsConfig, WireClient, WireResult, WireServer, WireServerConfig,
};
use tempfile::TempDir;

fn start(compression_enabled: bool, max_allowed_packet: usize) -> (TempDir, WireServer) {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    let wire = WireServer::start(
        WireServerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            compression_enabled,
            max_allowed_packet,
            read_timeout: Duration::from_millis(50),
            ..WireServerConfig::default()
        },
        server,
    )
    .unwrap();
    (dir, wire)
}

fn addr(wire: &WireServer) -> SocketAddr {
    wire.local_addr()
}

fn rows(result: WireResult) -> Vec<Row> {
    match result {
        WireResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn exercise_round_trip(compression: CompressionMode) {
    let (_dir, wire) = start(true, 64 * 1024 * 1024);
    let mut client = WireClient::connect_with(
        addr(&wire),
        ClientOptions {
            compression,
            ..ClientOptions::default()
        },
    )
    .unwrap();

    client
        .query("CREATE TABLE cmp (id INT PRIMARY KEY, v VARCHAR(32))")
        .unwrap();
    let values = (0..200)
        .map(|id| format!("({id}, 'value-{id:03}')"))
        .collect::<Vec<_>>()
        .join(", ");
    client
        .query(&format!("INSERT INTO cmp (id, v) VALUES {values}"))
        .unwrap();

    assert_eq!(
        rows(
            client
                .query("SELECT id, v FROM cmp WHERE id = 199")
                .unwrap()
        ),
        vec![Row::new(vec![
            Value::Int32(199),
            Value::String("value-199".into()),
        ])]
    );

    let stmt = client.prepare("SELECT v FROM cmp WHERE id = ?").unwrap();
    assert_eq!(
        rows(client.execute_prepared(&stmt, &[Value::Int32(42)]).unwrap()),
        vec![Row::new(vec![Value::String("value-042".into())])]
    );
    client.close_stmt(stmt.stmt_id).unwrap();
    wire.shutdown();
}

#[test]
fn test_wire_compression_zlib_round_trip() {
    exercise_round_trip(CompressionMode::Zlib);
}

#[test]
fn test_wire_compression_zstd_round_trip() {
    exercise_round_trip(CompressionMode::Zstd { level: 3 });
}

#[test]
fn test_wire_compression_large_payload_over_16mb_round_trip() {
    for compression in [CompressionMode::Zlib, CompressionMode::Zstd { level: 3 }] {
        let (_dir, wire) = start(true, 64 * 1024 * 1024);
        let mut client = WireClient::connect_with(
            addr(&wire),
            ClientOptions {
                compression,
                ..ClientOptions::default()
            },
        )
        .unwrap();

        let big = "x".repeat(17 * 1024 * 1024);
        assert_eq!(
            rows(client.query(&format!("SELECT '{big}'")).unwrap()),
            vec![Row::new(vec![Value::String(big)])]
        );
        wire.shutdown();
    }
}

#[test]
fn test_wire_compression_disabled_by_server_config() {
    let (_dir, wire) = start(false, 64 * 1024 * 1024);
    let mut client = WireClient::connect_with(
        addr(&wire),
        ClientOptions {
            compression: CompressionMode::Zlib,
            ..ClientOptions::default()
        },
    )
    .unwrap();
    assert_eq!(
        rows(client.query("SELECT 1").unwrap()),
        vec![Row::new(vec![Value::Int64(1)])]
    );
    wire.shutdown();
}

#[test]
fn test_compression_request_to_disabled_server_uses_uncompressed_connection() {
    let (_dir, wire) = start(false, 64 * 1024 * 1024);
    let mut tcp = TcpStream::connect(addr(&wire)).unwrap();

    let (_, payload) = read_packet(&mut tcp).unwrap();
    let handshake = HandshakeV10::decode(&payload).unwrap();
    let response = HandshakeResponse41 {
        capability_flags: CLIENT_PROTOCOL_41
            | CLIENT_SECURE_CONNECTION
            | CLIENT_PLUGIN_AUTH
            | CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA
            | CLIENT_DEPRECATE_EOF
            | CLIENT_COMPRESS,
        max_packet_size: 1 << 24,
        charset: COLLATION_UTF8MB4 as u8,
        username: "root".into(),
        auth_response: scramble_native_password(&handshake.scramble, b""),
        database: None,
        auth_plugin: Some(AUTH_PLUGIN_NATIVE.into()),
        zstd_compression_level: None,
    };
    write_packet(&mut tcp, 1, &response.encode()).unwrap();
    let (_, auth_ok) = read_packet(&mut tcp).unwrap();
    assert_eq!(auth_ok[0], OK_HEADER);

    write_packet(
        &mut tcp,
        0,
        &[COM_QUERY, b'S', b'E', b'L', b'E', b'C', b'T', b' ', b'1'],
    )
    .unwrap();
    let (_, column_count) = read_packet(&mut tcp).unwrap();
    assert_eq!(column_count, vec![1]);
    read_packet(&mut tcp).unwrap();
    let (_, row) = read_packet(&mut tcp).unwrap();
    assert_eq!(row, vec![1, b'1']);
    let (_, terminator) = read_packet(&mut tcp).unwrap();
    assert!(htap_wire::result_codec::is_resultset_terminator(
        &terminator
    ));

    wire.shutdown();
}

#[test]
fn test_wire_compression_zstd_level_22_round_trip() {
    exercise_round_trip(CompressionMode::Zstd { level: 22 });
}

#[test]
fn test_change_user_over_compressed_connection() {
    let (_dir, wire) = start(true, 64 * 1024 * 1024);
    let mut setup = WireClient::connect(addr(&wire), None).unwrap();
    setup
        .query("CREATE USER root2 IDENTIFIED BY 'second-password'")
        .unwrap();
    drop(setup);

    let mut tcp = TcpStream::connect(addr(&wire)).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

    let (_, payload) = read_packet(&mut tcp).unwrap();
    let handshake = HandshakeV10::decode(&payload).unwrap();
    let response = HandshakeResponse41 {
        capability_flags: CLIENT_PROTOCOL_41
            | CLIENT_SECURE_CONNECTION
            | CLIENT_PLUGIN_AUTH
            | CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA
            | CLIENT_DEPRECATE_EOF
            | CLIENT_COMPRESS,
        max_packet_size: 1 << 24,
        charset: COLLATION_UTF8MB4 as u8,
        username: "root".into(),
        auth_response: scramble_native_password(&handshake.scramble, b""),
        database: None,
        auth_plugin: Some(AUTH_PLUGIN_NATIVE.into()),
        zstd_compression_level: None,
    };
    write_packet(&mut tcp, 1, &response.encode()).unwrap();
    let (_, auth_ok) = read_packet(&mut tcp).unwrap();
    assert_eq!(auth_ok[0], OK_HEADER);

    let mut compressed =
        CompressedStream::new(tcp, Some(CompressionAlgorithm::Zlib), 64 * 1024 * 1024);
    let mut body = Vec::new();
    body.extend_from_slice(b"root2\0");
    let auth_response = scramble_native_password(&handshake.scramble, b"second-password");
    body.push(auth_response.len() as u8);
    body.extend_from_slice(&auth_response);
    body.push(0);
    body.extend_from_slice(&COLLATION_UTF8MB4.to_le_bytes());
    body.extend_from_slice(AUTH_PLUGIN_NATIVE.as_bytes());
    body.push(0);

    let mut command = vec![COM_CHANGE_USER];
    command.extend_from_slice(&body);
    write_packet(&mut compressed, 0, &command).unwrap();
    compressed.flush().unwrap();

    let (_, result) = read_packet(&mut compressed).unwrap();
    assert_eq!(result[0], OK_HEADER, "{result:?}");

    wire.shutdown();
}

#[test]
fn test_wire_compression_decompression_bomb_closes_connection() {
    let (_dir, wire) = start(true, 1024 * 1024);
    let mut tcp = TcpStream::connect(addr(&wire)).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

    let (_, payload) = read_packet(&mut tcp).unwrap();
    let handshake = HandshakeV10::decode(&payload).unwrap();
    let response = HandshakeResponse41 {
        capability_flags: CLIENT_PROTOCOL_41
            | CLIENT_SECURE_CONNECTION
            | CLIENT_PLUGIN_AUTH
            | CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA
            | CLIENT_COMPRESS,
        max_packet_size: 1 << 24,
        charset: COLLATION_UTF8MB4 as u8,
        username: "root".into(),
        auth_response: scramble_native_password(&handshake.scramble, b""),
        database: None,
        auth_plugin: Some(AUTH_PLUGIN_NATIVE.into()),
        zstd_compression_level: None,
    };
    write_packet(&mut tcp, 1, &response.encode()).unwrap();
    let (_, auth_ok) = read_packet(&mut tcp).unwrap();
    assert_eq!(auth_ok[0], OK_HEADER);

    let zeros = vec![0u8; 50 * 1024];
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&zeros).unwrap();
    let compressed = encoder.finish().unwrap();

    // Send a raw compressed frame with an intentionally false 8 MiB uncompressed length.
    let mut frame = vec![0; 7];
    let compressed_len = compressed.len();
    frame[0] = compressed_len as u8;
    frame[1] = (compressed_len >> 8) as u8;
    frame[2] = (compressed_len >> 16) as u8;
    frame[3] = 0;
    let declared = 8 * 1024 * 1024usize;
    frame[4] = declared as u8;
    frame[5] = (declared >> 8) as u8;
    frame[6] = (declared >> 16) as u8;
    frame.extend_from_slice(&compressed);
    tcp.write_all(&frame).unwrap();
    tcp.flush().unwrap();

    let mut response = [0u8; 1];
    let outcome = tcp.read(&mut response);
    assert!(
        matches!(outcome, Ok(0)) || outcome.is_err(),
        "bomb frame must close the connection or fail its read: {outcome:?}"
    );

    let mut healthy = WireClient::connect(addr(&wire), None).unwrap();
    assert_eq!(
        rows(healthy.query("SELECT 1").unwrap()),
        vec![Row::new(vec![Value::Int64(1)])]
    );
    wire.shutdown();
}

#[test]
fn test_mysql_crate_driver_interop_with_compression() {
    use mysql::prelude::*;

    let (_dir, wire) = start(true, 64 * 1024 * 1024);
    let opts = mysql::OptsBuilder::new()
        .ip_or_hostname(Some("127.0.0.1"))
        .tcp_port(addr(&wire).port())
        .user(Some("root"))
        .prefer_socket(false)
        .compress(Some(mysql::Compression::default()));
    let mut conn = mysql::Conn::new(opts).unwrap();

    let one: Option<i32> = conn.query_first("SELECT 1").unwrap();
    assert_eq!(one, Some(1));
    let stmt = conn.prep("SELECT ?").unwrap();
    let value: Option<i32> = conn.exec_first(&stmt, (42,)).unwrap();
    assert_eq!(value, Some(42));
    wire.shutdown();
}

#[test]
fn test_mysql_crate_driver_interop_with_tls_and_compression() {
    use mysql::prelude::*;

    let dir = TempDir::new().unwrap();
    let cert =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .unwrap();
    let cert_path = dir.path().join("server-cert.pem");
    let key_path = dir.path().join("server-key.pem");
    fs::write(&cert_path, cert.cert.pem()).unwrap();
    fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();

    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    let wire = WireServer::start(
        WireServerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            tls: Some(TlsConfig {
                cert_path: cert_path.clone(),
                key_path,
            }),
            read_timeout: Duration::from_millis(50),
            ..WireServerConfig::default()
        },
        server,
    )
    .unwrap();

    let opts = mysql::OptsBuilder::new()
        .ip_or_hostname(Some("127.0.0.1"))
        .tcp_port(addr(&wire).port())
        .user(Some("root"))
        .prefer_socket(false)
        .compress(Some(mysql::Compression::default()))
        .ssl_opts(mysql::SslOpts::default().with_root_cert_path(Some(cert_path)));
    let mut conn = mysql::Conn::new(opts).unwrap();

    let one: Option<i32> = conn.query_first("SELECT 1").unwrap();
    assert_eq!(one, Some(1));
    let stmt = conn.prep("SELECT ?").unwrap();
    let value: Option<i32> = conn.exec_first(&stmt, (42,)).unwrap();
    assert_eq!(value, Some(42));
    wire.shutdown();
}
