//! End-to-end tests for `WireServer` over real TCP sockets.

use std::collections::BTreeSet;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use htap_common::types::{DataType, Row, Value};
use htap_common::Version;
use htap_server::LocalServer;
use htap_wire::codec::{read_packet, write_packet};
use htap_wire::handshake::{AuthSwitchRequest, HandshakeResponse41, HandshakeV10};
use htap_wire::proto::*;
use htap_wire::result_codec::{is_resultset_terminator, parse_ok_payload};
use htap_wire::sha1::scramble_native_password;
use htap_wire::{ClientOptions, WireClient, WireError, WireResult, WireServer, WireServerConfig};
use tempfile::TempDir;

fn start(password: Option<&str>, max_connections: usize) -> (TempDir, WireServer) {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    let config = WireServerConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        max_connections,
        password: password.map(str::to_string),
        read_timeout: Duration::from_millis(50),
    };
    let wire = WireServer::start(config, server).unwrap();
    (dir, wire)
}

fn addr(wire: &WireServer) -> SocketAddr {
    wire.local_addr()
}

fn server_code(err: WireError) -> u16 {
    match err {
        WireError::Server { code, .. } => code,
        other => panic!("expected server error, got {other}"),
    }
}

fn rows(result: WireResult) -> Vec<Row> {
    match result {
        WireResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn ok(result: WireResult) -> htap_wire::OkPacket {
    match result {
        WireResult::Ok(ok) => ok,
        other => panic!("expected OK, got {other:?}"),
    }
}

#[test]
fn test_handshake_empty_password_ok() {
    let (_dir, wire) = start(None, 4);
    let client = WireClient::connect(addr(&wire), None).unwrap();
    assert_eq!(client.server_version(), SERVER_VERSION);
    assert!(client.deprecate_eof());
    // A password sent to a server without one is also accepted.
    let _c2 = WireClient::connect(addr(&wire), Some("anything")).unwrap();
    wire.shutdown();
}

#[test]
fn test_handshake_wrong_password_rejected_1045() {
    let (_dir, wire) = start(Some("secret"), 4);
    let err = WireClient::connect(addr(&wire), Some("wrong")).unwrap_err();
    assert_eq!(server_code(err), 1045);
    let err = WireClient::connect(addr(&wire), None).unwrap_err();
    assert_eq!(server_code(err), 1045);
    wire.shutdown();
}

#[test]
fn test_handshake_correct_password_ok() {
    let (_dir, wire) = start(Some("secret"), 4);
    let mut client = WireClient::connect(addr(&wire), Some("secret")).unwrap();
    client.ping().unwrap();
    wire.shutdown();
}

#[test]
fn test_auth_switch_to_native_password() {
    let (_dir, wire) = start(Some("secret"), 4);
    let mut stream = TcpStream::connect(addr(&wire)).unwrap();
    let (_, payload) = read_packet(&mut stream).unwrap();
    let hs = HandshakeV10::decode(&payload).unwrap();
    let response = HandshakeResponse41 {
        capability_flags: CLIENT_PROTOCOL_41
            | CLIENT_SECURE_CONNECTION
            | CLIENT_PLUGIN_AUTH
            | CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA,
        max_packet_size: 1 << 24,
        charset: 45,
        username: "root".into(),
        auth_response: vec![1, 2, 3],
        database: None,
        auth_plugin: Some("caching_sha2_password".into()),
    };
    write_packet(&mut stream, 1, &response.encode()).unwrap();
    let (seq, payload) = read_packet(&mut stream).unwrap();
    assert_eq!(seq, 2);
    let switch = AuthSwitchRequest::decode(&payload).unwrap();
    assert_eq!(switch.plugin, AUTH_PLUGIN_NATIVE);
    assert_ne!(switch.scramble, hs.scramble);
    let resp = scramble_native_password(&switch.scramble, b"secret");
    write_packet(&mut stream, 3, &resp).unwrap();
    let (seq, payload) = read_packet(&mut stream).unwrap();
    assert_eq!(seq, 4);
    assert_eq!(payload[0], OK_HEADER);
    wire.shutdown();
}

#[test]
fn test_ssl_request_rejected_and_pre41_rejected() {
    let (_dir, wire) = start(None, 4);
    let mut stream = TcpStream::connect(addr(&wire)).unwrap();
    let _ = read_packet(&mut stream).unwrap();
    let mut ssl = Vec::new();
    ssl.extend_from_slice(&(CLIENT_PROTOCOL_41 | CLIENT_SSL).to_le_bytes());
    ssl.extend_from_slice(&(1u32 << 24).to_le_bytes());
    ssl.push(45);
    ssl.extend_from_slice(&[0u8; 23]);
    write_packet(&mut stream, 1, &ssl).unwrap();
    let (_, payload) = read_packet(&mut stream).unwrap();
    assert_eq!(payload[0], ERR_HEADER);

    let mut stream = TcpStream::connect(addr(&wire)).unwrap();
    let _ = read_packet(&mut stream).unwrap();
    write_packet(&mut stream, 1, &[0u8; 40]).unwrap();
    let (_, payload) = read_packet(&mut stream).unwrap();
    assert_eq!(payload[0], ERR_HEADER);
    wire.shutdown();
}

#[test]
fn test_ddl_insert_point_select_round_trip() {
    let (_dir, wire) = start(None, 4);
    let mut c = WireClient::connect(addr(&wire), None).unwrap();
    let r = ok(c
        .query("CREATE TABLE users (id BIGINT PRIMARY KEY, name VARCHAR(64))")
        .unwrap());
    assert_eq!(r.affected_rows, 1);
    assert_eq!(r.info, "");
    let r = ok(c
        .query("INSERT INTO users (id, name) VALUES (1, 'ann'), (2, 'bob')")
        .unwrap());
    assert_eq!(r.affected_rows, 2);
    let first_version: u64 = r.info.strip_prefix("version=").unwrap().parse().unwrap();
    let rs = c.query("SELECT id, name FROM users WHERE id = 2").unwrap();
    match rs {
        WireResult::Rows { columns, rows } => {
            assert_eq!(columns.len(), 2);
            assert_eq!(columns[0].name, "id");
            assert_eq!(columns[0].data_type, DataType::Int64);
            assert!(columns[0].primary_key);
            assert_eq!(columns[1].data_type, DataType::String);
            assert_eq!(
                rows,
                vec![Row::new(vec![Value::Int64(2), Value::String("bob".into())])]
            );
        }
        other => panic!("{other:?}"),
    }
    let rs = rows(c.query("SELECT * FROM users WHERE id = 99").unwrap());
    assert!(rs.is_empty());
    let r = ok(c.query("DELETE FROM users WHERE id = 1").unwrap());
    assert_eq!(r.affected_rows, 1);
    assert_eq!(r.info, format!("version={}", first_version + 1));
    c.quit().unwrap();
    wire.shutdown();
}

#[test]
fn test_analytic_select_round_trip() {
    let (_dir, wire) = start(None, 4);
    let mut c = WireClient::connect(addr(&wire), None).unwrap();
    c.query("CREATE TABLE m (id INT PRIMARY KEY, grp VARCHAR(8), v BIGINT)")
        .unwrap();
    c.query(
        "INSERT INTO m (id, grp, v) VALUES (1, 'a', 10), (2, 'a', 20), (3, 'b', 5), (4, 'b', NULL)",
    )
    .unwrap();
    let rs = c
        .query("SELECT grp, COUNT(*), SUM(v) FROM m GROUP BY grp ORDER BY grp")
        .unwrap();
    assert_eq!(
        rows(rs),
        vec![
            Row::new(vec![
                Value::String("a".into()),
                Value::Int64(2),
                Value::Int64(30)
            ]),
            Row::new(vec![
                Value::String("b".into()),
                Value::Int64(2),
                Value::Int64(5)
            ]),
        ]
    );
    let rs = rows(
        c.query("SELECT id FROM m WHERE v > 5 ORDER BY id DESC")
            .unwrap(),
    );
    assert_eq!(
        rs,
        vec![
            Row::new(vec![Value::Int32(2)]),
            Row::new(vec![Value::Int32(1)])
        ]
    );
    wire.shutdown();
}

#[test]
fn test_typed_values_null_bytes_float_timestamp_round_trip() {
    let (_dir, wire) = start(None, 4);
    let mut c = WireClient::connect(addr(&wire), None).unwrap();
    c.query(
        "CREATE TABLE t (id INT PRIMARY KEY, b BOOL, f DOUBLE, s VARCHAR(32), \
         raw VARBINARY(16), ts TIMESTAMP)",
    )
    .unwrap();
    c.query(
        "INSERT INTO t (id, b, f, s, raw, ts) VALUES (1, TRUE, -1.5, 'héllo', X'00ff10', 1700000000123456), \
         (2, NULL, NULL, NULL, NULL, NULL)",
    )
    .unwrap();
    let rs = rows(c.query("SELECT * FROM t WHERE id = 1").unwrap());
    assert_eq!(
        rs,
        vec![Row::new(vec![
            Value::Int32(1),
            Value::Bool(true),
            Value::Float64(-1.5),
            Value::String("héllo".into()),
            Value::Bytes(vec![0x00, 0xff, 0x10]),
            Value::Timestamp(1_700_000_000_123_456),
        ])]
    );
    let rs = rows(c.query("SELECT * FROM t WHERE id = 2").unwrap());
    assert_eq!(
        rs,
        vec![Row::new(vec![
            Value::Int32(2),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ])]
    );

    // Bytes travel raw on the wire, not hex-encoded: inspect the raw row packet.
    let mut stream = TcpStream::connect(addr(&wire)).unwrap();
    let (_, payload) = read_packet(&mut stream).unwrap();
    let hs = HandshakeV10::decode(&payload).unwrap();
    let response = HandshakeResponse41 {
        capability_flags: CLIENT_PROTOCOL_41
            | CLIENT_SECURE_CONNECTION
            | CLIENT_PLUGIN_AUTH
            | CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA
            | CLIENT_DEPRECATE_EOF,
        max_packet_size: 1 << 24,
        charset: 45,
        username: "root".into(),
        auth_response: scramble_native_password(&hs.scramble, b""),
        database: None,
        auth_plugin: Some(AUTH_PLUGIN_NATIVE.into()),
    };
    write_packet(&mut stream, 1, &response.encode()).unwrap();
    let (_, okp) = read_packet(&mut stream).unwrap();
    assert_eq!(okp[0], OK_HEADER);
    let mut q = vec![COM_QUERY];
    q.extend_from_slice(b"SELECT raw FROM t WHERE id = 1");
    write_packet(&mut stream, 0, &q).unwrap();
    let (_, count) = read_packet(&mut stream).unwrap();
    assert_eq!(count, vec![1]);
    let _def = read_packet(&mut stream).unwrap();
    let (_, row) = read_packet(&mut stream).unwrap();
    assert_eq!(
        row,
        vec![3, 0x00, 0xff, 0x10],
        "bytes must be raw, not hex text"
    );
    let (_, term) = read_packet(&mut stream).unwrap();
    assert!(is_resultset_terminator(&term));
    wire.shutdown();
}

#[test]
fn test_syntax_error_maps_to_1064() {
    let (_dir, wire) = start(None, 4);
    let mut c = WireClient::connect(addr(&wire), None).unwrap();
    let err = c.query("SELEC nonsense").unwrap_err();
    assert_eq!(server_code(err), 1064);
    // The connection is still usable afterwards.
    c.ping().unwrap();
    wire.shutdown();
}

#[test]
fn test_missing_table_maps_to_1146() {
    let (_dir, wire) = start(None, 4);
    let mut c = WireClient::connect(addr(&wire), None).unwrap();
    let err = c.query("SELECT id FROM nope WHERE id = 1").unwrap_err();
    match err {
        WireError::Server { code, sqlstate, .. } => {
            assert_eq!(code, 1146);
            assert_eq!(sqlstate, "42S02");
        }
        other => panic!("{other}"),
    }
    wire.shutdown();
}

#[test]
fn test_too_many_connections_returns_1040() {
    let (_dir, wire) = start(None, 1);
    let mut first = WireClient::connect(addr(&wire), None).unwrap();
    first.ping().unwrap();
    let err = WireClient::connect(addr(&wire), None).unwrap_err();
    assert_eq!(server_code(err), 1040);
    drop(first);
    // After the first connection closes, a new one is accepted.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match WireClient::connect(addr(&wire), None) {
            Ok(_) => break,
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(e) => panic!("connection slot never freed: {e}"),
        }
    }
    wire.shutdown();
}

#[test]
fn test_concurrent_connections_dense_versions() {
    let (_dir, wire) = start(None, 32);
    let mut c = WireClient::connect(addr(&wire), None).unwrap();
    c.query("CREATE TABLE k (id BIGINT PRIMARY KEY, v BIGINT)")
        .unwrap();
    let n = 8;
    let per = 5;
    let a = addr(&wire);
    let handles: Vec<_> = (0..n)
        .map(|t| {
            std::thread::spawn(move || {
                let mut c = WireClient::connect(a, None).unwrap();
                let mut versions = Vec::new();
                for i in 0..per {
                    let id = t * 100 + i;
                    let r = ok(c
                        .query(&format!("INSERT INTO k (id, v) VALUES ({id}, {i})"))
                        .unwrap());
                    let v: u64 = r.info.strip_prefix("version=").unwrap().parse().unwrap();
                    versions.push(v);
                }
                versions
            })
        })
        .collect();
    let mut all: BTreeSet<u64> = BTreeSet::new();
    for h in handles {
        for v in h.join().unwrap() {
            assert!(all.insert(v), "duplicate version {v}");
        }
    }
    assert_eq!(all.len(), n * per);
    let min = *all.iter().next().unwrap();
    let max = *all.iter().next_back().unwrap();
    assert_eq!(max - min + 1, (n * per) as u64, "versions must be dense");
    let count = rows(c.query("SELECT COUNT(*) FROM k").unwrap());
    assert_eq!(count, vec![Row::new(vec![Value::Int64((n * per) as i64)])]);
    wire.shutdown();
}

#[test]
fn test_com_ping() {
    let (_dir, wire) = start(None, 4);
    let mut c = WireClient::connect(addr(&wire), None).unwrap();
    for _ in 0..3 {
        c.ping().unwrap();
    }
    wire.shutdown();
}

#[test]
fn test_com_init_db_known_and_unknown_db() {
    let (_dir, wire) = start(None, 4);
    let mut c = WireClient::connect(addr(&wire), None).unwrap();
    c.init_db("htap").unwrap();
    c.init_db("fluidb").unwrap();
    let err = c.init_db("other").unwrap_err();
    assert_eq!(server_code(err), 1049);
    let err = c.query("USE other").unwrap_err();
    assert_eq!(server_code(err), 1049);
    ok(c.query("USE htap").unwrap());
    // Connecting with a database name goes through the same validation.
    let opts = ClientOptions {
        database: Some("nope".into()),
        ..ClientOptions::default()
    };
    let err = WireClient::connect_with(addr(&wire), opts).unwrap_err();
    assert_eq!(server_code(err), 1049);
    let opts = ClientOptions {
        database: Some("htap".into()),
        ..ClientOptions::default()
    };
    WireClient::connect_with(addr(&wire), opts).unwrap();
    wire.shutdown();
}

#[test]
fn test_shim_set_and_version_comment() {
    let (_dir, wire) = start(None, 4);
    let mut c = WireClient::connect(addr(&wire), None).unwrap();
    let r = ok(c.query("SET NAMES utf8mb4").unwrap());
    assert_eq!(r.affected_rows, 0);
    let rs = rows(c.query("select @@version_comment limit 1").unwrap());
    assert_eq!(rs, vec![Row::new(vec![Value::String("fluidb".into())])]);
    let rs = rows(c.query("SELECT @@max_allowed_packet, @@socket").unwrap());
    assert_eq!(
        rs,
        vec![Row::new(vec![Value::Int64(16 * 1024 * 1024), Value::Null])]
    );
    let rs = rows(c.query("SELECT VERSION()").unwrap());
    assert_eq!(
        rs,
        vec![Row::new(vec![Value::String(SERVER_VERSION.into())])]
    );
    // Unknown system variables fall through to the engine and fail as SQL.
    let err = c.query("SELECT @@does_not_exist").unwrap_err();
    assert!(matches!(err, WireError::Server { .. }));
    wire.shutdown();
}

#[test]
fn test_shutdown_joins_and_frees_port() {
    let (_dir, wire) = start(None, 4);
    let a = addr(&wire);
    let mut idle = WireClient::connect(a, None).unwrap();
    idle.ping().unwrap();
    wire.shutdown();
    // The port can be rebound immediately and the idle connection is gone.
    let _rebind = TcpListener::bind(a).unwrap();
    assert!(idle.ping().is_err());
}

#[test]
fn test_unknown_command_returns_1047() {
    let (_dir, wire) = start(None, 4);
    let mut c = WireClient::connect(addr(&wire), None).unwrap();
    let payload = c.send_raw_command(0x7f, &[]).unwrap();
    assert_eq!(payload[0], ERR_HEADER);
    let (code, _, _) = htap_wire::error_map::parse_err_payload(&payload).unwrap();
    assert_eq!(code, 1047);
    c.ping().unwrap();
    wire.shutdown();
}

#[test]
fn test_prepared_statement_command_rejected_cleanly() {
    let (_dir, wire) = start(None, 4);
    let mut c = WireClient::connect(addr(&wire), None).unwrap();
    for cmd in [
        COM_STMT_PREPARE,
        COM_STMT_EXECUTE,
        COM_RESET_CONNECTION,
        COM_CHANGE_USER,
    ] {
        let payload = c.send_raw_command(cmd, b"SELECT 1").unwrap();
        let (code, _, _) = htap_wire::error_map::parse_err_payload(&payload).unwrap();
        assert_eq!(code, 1047, "command 0x{cmd:02x}");
    }
    c.ping().unwrap();
    wire.shutdown();
}

#[test]
fn test_oversized_packet_closes_connection() {
    let (_dir, wire) = start(None, 4);
    let mut c = WireClient::connect(addr(&wire), None).unwrap();
    c.ping().unwrap();
    // Forge a 16MB length header; the server must drop the connection.
    let mut stream = TcpStream::connect(addr(&wire)).unwrap();
    let _ = read_packet(&mut stream).unwrap();
    use std::io::Write;
    stream.write_all(&[0xff, 0xff, 0xff, 1]).unwrap();
    let (_, payload) = read_packet(&mut stream).unwrap_or((0, vec![ERR_HEADER]));
    assert!(payload.is_empty() || payload[0] == ERR_HEADER || read_packet(&mut stream).is_err());
    wire.shutdown();
}

#[test]
fn test_legacy_eof_terminator_used_when_client_does_not_negotiate_deprecate_eof() {
    let (_dir, wire) = start(None, 4);
    let opts = ClientOptions {
        deprecate_eof: false,
        ..ClientOptions::default()
    };
    let mut c = WireClient::connect_with(addr(&wire), opts).unwrap();
    assert!(!c.deprecate_eof());
    c.query("CREATE TABLE e (id INT PRIMARY KEY, v BIGINT)")
        .unwrap();
    c.query("INSERT INTO e (id, v) VALUES (1, 10), (2, 20)")
        .unwrap();
    let rs = rows(c.query("SELECT id, v FROM e ORDER BY id").unwrap());
    assert_eq!(
        rs,
        vec![
            Row::new(vec![Value::Int32(1), Value::Int64(10)]),
            Row::new(vec![Value::Int32(2), Value::Int64(20)]),
        ]
    );
    let empty = rows(c.query("SELECT id FROM e WHERE id = 7").unwrap());
    assert!(empty.is_empty());
    let err = c.query("SELECT id FROM nope WHERE id = 1").unwrap_err();
    assert_eq!(server_code(err), 1146);
    c.ping().unwrap();
    wire.shutdown();
}

#[test]
fn test_resultset_packets_modern_vs_legacy() {
    // Drive the raw stream by hand to check the exact terminator packets.
    let (_dir, wire) = start(None, 4);
    let mut setup = WireClient::connect(addr(&wire), None).unwrap();
    setup.query("CREATE TABLE p (id INT PRIMARY KEY)").unwrap();
    setup.query("INSERT INTO p (id) VALUES (1)").unwrap();

    for deprecate in [true, false] {
        let mut stream = TcpStream::connect(addr(&wire)).unwrap();
        let (_, payload) = read_packet(&mut stream).unwrap();
        let hs = HandshakeV10::decode(&payload).unwrap();
        let mut caps = CLIENT_PROTOCOL_41
            | CLIENT_SECURE_CONNECTION
            | CLIENT_PLUGIN_AUTH
            | CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA;
        if deprecate {
            caps |= CLIENT_DEPRECATE_EOF;
        }
        let response = HandshakeResponse41 {
            capability_flags: caps,
            max_packet_size: 1 << 24,
            charset: 45,
            username: "root".into(),
            auth_response: scramble_native_password(&hs.scramble, b""),
            database: None,
            auth_plugin: Some(AUTH_PLUGIN_NATIVE.into()),
        };
        write_packet(&mut stream, 1, &response.encode()).unwrap();
        let (_, okp) = read_packet(&mut stream).unwrap();
        assert_eq!(okp[0], OK_HEADER);

        let mut q = vec![COM_QUERY];
        q.extend_from_slice(b"SELECT id FROM p WHERE id = 1");
        write_packet(&mut stream, 0, &q).unwrap();
        let (s, count) = read_packet(&mut stream).unwrap();
        assert_eq!((s, count), (1, vec![1]));
        let (s, _def) = read_packet(&mut stream).unwrap();
        assert_eq!(s, 2);
        let mut expected_seq = 3;
        if !deprecate {
            let (s, eof) = read_packet(&mut stream).unwrap();
            assert_eq!(s, expected_seq);
            assert_eq!(eof, vec![EOF_HEADER, 0, 0, 2, 0]);
            expected_seq += 1;
        }
        let (s, row) = read_packet(&mut stream).unwrap();
        assert_eq!(s, expected_seq);
        assert_eq!(row, vec![1, b'1']);
        let (s, term) = read_packet(&mut stream).unwrap();
        assert_eq!(s, expected_seq + 1);
        assert_eq!(term[0], EOF_HEADER);
        assert!(is_resultset_terminator(&term));
        if deprecate {
            let parsed = parse_ok_payload(&term).unwrap();
            assert_eq!(parsed.status, SERVER_STATUS_AUTOCOMMIT);
        } else {
            assert_eq!(term.len(), 5);
        }
    }
    wire.shutdown();
}

#[test]
fn test_dml_versions_are_reported() {
    let (_dir, wire) = start(None, 4);
    let mut c = WireClient::connect(addr(&wire), None).unwrap();
    c.query("CREATE TABLE v (id INT PRIMARY KEY)").unwrap();
    let r = ok(c.query("INSERT INTO v (id) VALUES (1)").unwrap());
    let v1: u64 = r.info.strip_prefix("version=").unwrap().parse().unwrap();
    let r = ok(c.query("INSERT INTO v (id) VALUES (2)").unwrap());
    assert_eq!(r.info, format!("version={}", Version::new(v1 + 1).get()));
    wire.shutdown();
}

/// Real-driver interoperability: the `mysql` crate (pure Rust) connects, runs DDL/DML and
/// reads a text result set. This driver does not request `CLIENT_DEPRECATE_EOF`, so it
/// exercises the legacy EOF path. `max_allowed_packet` and `prefer_socket` are set explicitly
/// so the driver skips its start-up probes.
#[test]
fn test_mysql_crate_driver_interop() {
    use mysql::prelude::*;
    let (_dir, wire) = start(Some("pw"), 4);
    let opts = mysql::OptsBuilder::new()
        .ip_or_hostname(Some("127.0.0.1"))
        .tcp_port(addr(&wire).port())
        .user(Some("root"))
        .pass(Some("pw"))
        .prefer_socket(false)
        .max_allowed_packet(Some(16 * 1024 * 1024));
    let mut conn = mysql::Conn::new(opts).unwrap();
    conn.ping().unwrap();
    conn.query_drop("CREATE TABLE drv (id BIGINT PRIMARY KEY, name VARCHAR(32), score DOUBLE)")
        .unwrap();
    conn.query_drop("INSERT INTO drv (id, name, score) VALUES (1, 'ann', 1.5), (2, 'bob', 2.5)")
        .unwrap();
    assert_eq!(conn.affected_rows(), 2);
    let got: Vec<(i64, String, f64)> = conn
        .query("SELECT id, name, score FROM drv ORDER BY id")
        .unwrap();
    assert_eq!(got, vec![(1, "ann".into(), 1.5), (2, "bob".into(), 2.5)]);
    let one: Option<(i64, String)> = conn
        .query_first("SELECT id, name FROM drv WHERE id = 2")
        .unwrap();
    assert_eq!(one, Some((2, "bob".into())));
    let err = conn
        .query_drop("SELECT * FROM missing WHERE id = 1")
        .unwrap_err();
    match err {
        mysql::Error::MySqlError(e) => assert_eq!(e.code, 1146),
        other => panic!("{other}"),
    }
    conn.query_drop("SET NAMES utf8mb4").unwrap();
    drop(conn);

    let bad = mysql::OptsBuilder::new()
        .ip_or_hostname(Some("127.0.0.1"))
        .tcp_port(addr(&wire).port())
        .user(Some("root"))
        .pass(Some("nope"))
        .prefer_socket(false)
        .max_allowed_packet(Some(16 * 1024 * 1024));
    match mysql::Conn::new(bad) {
        Err(mysql::Error::MySqlError(e)) => assert_eq!(e.code, 1045),
        other => panic!("expected access denied, got {other:?}"),
    }
    wire.shutdown();
}

/// Joins, UPDATE, SHOW/DESCRIBE and DROP TABLE round-trip through the wire protocol.
#[test]
fn test_general_sql_over_wire() {
    let (_dir, wire) = start(None, 4);
    let mut c = WireClient::connect(addr(&wire), None).unwrap();
    c.query("CREATE TABLE u (id INT PRIMARY KEY, name VARCHAR(16))")
        .unwrap();
    c.query("CREATE TABLE o (oid BIGINT PRIMARY KEY, uid INT, amt DOUBLE)")
        .unwrap();
    c.query("INSERT INTO u (id, name) VALUES (1, 'ann'), (2, 'bob')")
        .unwrap();
    c.query("INSERT INTO o (oid, uid, amt) VALUES (10, 1, 2.5), (11, 1, 4.0), (12, 3, 1.0)")
        .unwrap();
    let rs = rows(
        c.query(
            "SELECT u.name, COUNT(o.oid) AS n, COALESCE(SUM(o.amt), 0) AS total \
             FROM u LEFT JOIN o ON o.uid = u.id GROUP BY u.name ORDER BY total DESC LIMIT 5",
        )
        .unwrap(),
    );
    assert_eq!(
        rs,
        vec![
            Row::new(vec![
                Value::String("ann".into()),
                Value::Int64(2),
                Value::Float64(6.5)
            ]),
            Row::new(vec![
                Value::String("bob".into()),
                Value::Int64(0),
                Value::Float64(0.0)
            ]),
        ]
    );
    let r = ok(c.query("UPDATE o SET amt = amt * 2 WHERE uid = 1").unwrap());
    assert_eq!(r.affected_rows, 2);
    assert!(r.info.starts_with("version="));
    let names = rows(c.query("SHOW TABLES").unwrap());
    assert_eq!(
        names,
        vec![
            Row::new(vec![Value::String("o".into())]),
            Row::new(vec![Value::String("u".into())])
        ]
    );
    let desc = rows(c.query("DESCRIBE u").unwrap());
    assert_eq!(desc.len(), 2);
    assert_eq!(desc[0].get(0), Some(&Value::String("id".into())));
    assert_eq!(desc[0].get(3), Some(&Value::String("PRI".into())));
    let r = ok(c.query("DROP TABLE o").unwrap());
    assert_eq!(r.affected_rows, 1);
    let err = c.query("SELECT COUNT(*) FROM o").unwrap_err();
    assert_eq!(server_code(err), 1146);
    wire.shutdown();
}
