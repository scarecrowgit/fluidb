//! End-to-end tests for `WireServer` over real TCP sockets.

use std::collections::BTreeSet;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use htap_common::types::{ColumnDef, DataType, Row, Value};
use htap_common::{HtapError, Version};
use htap_server::LocalServer;
use htap_wire::binary_codec::{decode_stmt_prepare_ok, StmtPrepareOk};
use htap_wire::codec::{
    read_lenenc_int, read_lenenc_str, read_packet, write_packet, MAX_PACKET_PAYLOAD,
};
use htap_wire::handshake::{AuthSwitchRequest, HandshakeResponse41, HandshakeV10};
use htap_wire::proto::*;
use htap_wire::result_codec::{
    is_resultset_terminator, mysql_type_for, parse_column_def41, parse_ok_payload,
    parse_terminator_status,
};
use htap_wire::sha1::scramble_native_password;
use htap_wire::{ClientOptions, WireClient, WireError, WireResult, WireServer, WireServerConfig};
use tempfile::TempDir;

fn start(password: Option<&str>, max_connections: usize) -> (TempDir, WireServer) {
    let (dir, _server, wire) = start_with_server(password, max_connections);
    (dir, wire)
}

/// Like [`start`], but also returns the underlying [`LocalServer`] so a test can install a
/// `TransactionManager` commit hook (`test_prepared_statement_in_transaction_and_commit_outcome_pending`
/// and the `COM_RESET_CONNECTION`/`COM_QUIT`-under-quarantine tests): the Phase 10 wire tests
/// never needed this, since none of them drove the session into `CommitOutcomePending`.
fn start_with_server(
    password: Option<&str>,
    max_connections: usize,
) -> (TempDir, Arc<LocalServer>, WireServer) {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    let config = WireServerConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        max_connections,
        password: password.map(str::to_string),
        read_timeout: Duration::from_millis(50),
        ..WireServerConfig::default()
    };
    let wire = WireServer::start(config, Arc::clone(&server)).unwrap();
    (dir, server, wire)
}

/// Like [`start_with_server`], but with an explicit `max_allowed_packet` (Phase 11 plan task 8).
fn start_with_max_allowed_packet(max_allowed_packet: usize) -> (TempDir, WireServer) {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    let config = WireServerConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        read_timeout: Duration::from_millis(50),
        max_allowed_packet,
        ..WireServerConfig::default()
    };
    let wire = WireServer::start(config, server).unwrap();
    (dir, wire)
}

fn addr(wire: &WireServer) -> SocketAddr {
    wire.local_addr()
}

/// Performs a raw handshake (no password, `CLIENT_DEPRECATE_EOF` negotiated) over `stream` and
/// leaves it ready for the first command packet at sequence id 0.
fn raw_handshake(stream: &mut TcpStream) {
    raw_handshake_with_password(stream, b"");
}

/// Performs a raw handshake with `password` and returns the scramble the server sent, which a
/// caller building a later `COM_CHANGE_USER` request needs (real clients, confirmed against
/// `mysql-28.0.2`, reuse this same scramble for that request's auth response; see
/// `htap_wire::server`'s `Session::scramble` doc comment).
fn raw_handshake_with_password(stream: &mut TcpStream, password: &[u8]) -> [u8; 20] {
    let (_, payload) = read_packet(stream).unwrap();
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
        auth_response: scramble_native_password(&hs.scramble, password),
        database: None,
        auth_plugin: Some(AUTH_PLUGIN_NATIVE.into()),
    };
    write_packet(stream, 1, &response.encode()).unwrap();
    let (_, okp) = read_packet(stream).unwrap();
    assert_eq!(okp[0], OK_HEADER, "{okp:?}");
    hs.scramble
}

/// Like [`raw_handshake`], but also negotiates `CLIENT_MULTI_STATEMENTS`/`CLIENT_MULTI_RESULTS`
/// (Phase 11 plan task 10).
fn raw_handshake_multi_statements(stream: &mut TcpStream) {
    let (_, payload) = read_packet(stream).unwrap();
    let hs = HandshakeV10::decode(&payload).unwrap();
    let response = HandshakeResponse41 {
        capability_flags: CLIENT_PROTOCOL_41
            | CLIENT_SECURE_CONNECTION
            | CLIENT_PLUGIN_AUTH
            | CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA
            | CLIENT_DEPRECATE_EOF
            | CLIENT_MULTI_STATEMENTS
            | CLIENT_MULTI_RESULTS,
        max_packet_size: 1 << 24,
        charset: 45,
        username: "root".into(),
        auth_response: scramble_native_password(&hs.scramble, b""),
        database: None,
        auth_plugin: Some(AUTH_PLUGIN_NATIVE.into()),
    };
    write_packet(stream, 1, &response.encode()).unwrap();
    let (_, okp) = read_packet(stream).unwrap();
    assert_eq!(okp[0], OK_HEADER, "{okp:?}");
}

/// Sends one command packet at sequence id 0.
fn raw_send_command(stream: &mut TcpStream, command: u8, body: &[u8]) {
    let mut payload = vec![command];
    payload.extend_from_slice(body);
    write_packet(stream, 0, &payload).unwrap();
}

/// Sends a `COM_STMT_PREPARE` and reads the whole response, returning the fixed header and any
/// resolved output columns. `Err` carries `(code, sqlstate, message)` from an ERR response.
fn raw_prepare_result(
    stream: &mut TcpStream,
    sql: &str,
) -> Result<(StmtPrepareOk, Vec<ColumnDef>), (u16, String, String)> {
    raw_send_command(stream, COM_STMT_PREPARE, sql.as_bytes());
    let (_, first) = read_packet(stream).unwrap();
    if first[0] == ERR_HEADER {
        return Err(htap_wire::error_map::parse_err_payload(&first).unwrap());
    }
    let prepare_ok = decode_stmt_prepare_ok(&first).unwrap();
    for _ in 0..prepare_ok.num_params {
        read_packet(stream).unwrap();
    }
    if prepare_ok.num_params > 0 {
        read_packet(stream).unwrap(); // terminator
    }
    let mut columns = Vec::with_capacity(prepare_ok.num_columns as usize);
    for _ in 0..prepare_ok.num_columns {
        let (_, def) = read_packet(stream).unwrap();
        columns.push(parse_column_def41(&def).unwrap());
    }
    if prepare_ok.num_columns > 0 {
        read_packet(stream).unwrap(); // terminator
    }
    Ok((prepare_ok, columns))
}

fn raw_prepare(stream: &mut TcpStream, sql: &str) -> (StmtPrepareOk, Vec<ColumnDef>) {
    raw_prepare_result(stream, sql).unwrap_or_else(|e| panic!("PREPARE {sql:?} failed: {e:?}"))
}

/// Builds a `COM_STMT_EXECUTE` payload body (after the leading command byte) from
/// `(mysql_type, unsigned, value_bytes)` triples, `None` meaning NULL.
fn build_execute_payload(
    stmt_id: u32,
    params: &[(u8, bool, Option<Vec<u8>>)],
    new_params_bound: bool,
) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&stmt_id.to_le_bytes());
    buf.push(CURSOR_TYPE_NO_CURSOR);
    buf.extend_from_slice(&1u32.to_le_bytes());
    if !params.is_empty() {
        let bitmap_len = params.len().div_ceil(8);
        let mut bitmap = vec![0u8; bitmap_len];
        for (i, (_, _, v)) in params.iter().enumerate() {
            if v.is_none() {
                bitmap[i / 8] |= 1 << (i % 8);
            }
        }
        buf.extend_from_slice(&bitmap);
        buf.push(u8::from(new_params_bound));
        if new_params_bound {
            for (t, unsigned, _) in params {
                buf.push(*t);
                buf.push(if *unsigned { 0x80 } else { 0 });
            }
        }
        for (_, _, v) in params {
            if let Some(bytes) = v {
                buf.extend_from_slice(bytes);
            }
        }
    }
    buf
}

/// Length-encodes `s` the way a `COM_STMT_EXECUTE` payload's `VAR_STRING`/`BLOB`-family parameter
/// values are framed (`build_execute_payload` appends its `Some(bytes)` argument verbatim, so a
/// caller building such a parameter must length-encode it first).
fn lenenc_bytes(s: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    htap_wire::codec::write_lenenc_str(&mut buf, s);
    buf
}

/// Sends a `COM_STMT_EXECUTE` and returns the first response packet (an OK, ERR, or the start of
/// a resultset).
fn raw_execute(stream: &mut TcpStream, payload: &[u8]) -> Vec<u8> {
    raw_send_command(stream, COM_STMT_EXECUTE, payload);
    read_packet(stream).unwrap().1
}

/// Sends a `COM_STMT_EXECUTE` expected to produce a query resultset and reads it fully, returning
/// the column definitions and each row's raw binary payload (undecoded: callers use
/// [`decode_binary_row`] for the handful of types a given test needs).
fn raw_execute_query(stream: &mut TcpStream, payload: &[u8]) -> (Vec<ColumnDef>, Vec<Vec<u8>>) {
    raw_send_command(stream, COM_STMT_EXECUTE, payload);
    let (_, first) = read_packet(stream).unwrap();
    assert!(
        first[0] != OK_HEADER && first[0] != ERR_HEADER,
        "expected a resultset, got {first:?}"
    );
    let mut pos = 0;
    let col_count = read_lenenc_int(&first, &mut pos).unwrap() as usize;
    let mut columns = Vec::with_capacity(col_count);
    for _ in 0..col_count {
        let (_, def) = read_packet(stream).unwrap();
        columns.push(parse_column_def41(&def).unwrap());
    }
    // `raw_handshake` always negotiates `CLIENT_DEPRECATE_EOF`, so there is no legacy mid-EOF to
    // skip here (see `encode_resultset`'s doc comment).
    let mut rows = Vec::new();
    loop {
        let (_, row_payload) = read_packet(stream).unwrap();
        if is_resultset_terminator(&row_payload) {
            break;
        }
        rows.push(row_payload);
    }
    (columns, rows)
}

/// Decodes a `COM_STMT_EXECUTE` binary resultset row for the subset of types this test file
/// exercises directly over raw packets (the `mysql`-crate-based interop test decodes the rest via
/// the real driver instead).
fn decode_binary_row(payload: &[u8], columns: &[ColumnDef]) -> Row {
    use htap_wire::codec::read_fixed;
    let mut pos = 0;
    assert_eq!(payload[pos], 0x00);
    pos += 1;
    let bitmap_len = (columns.len() + 9) / 8;
    let bitmap = &payload[pos..pos + bitmap_len];
    pos += bitmap_len;
    let mut values = Vec::with_capacity(columns.len());
    for (i, c) in columns.iter().enumerate() {
        let bit_pos = i + 2;
        if bitmap[bit_pos / 8] & (1 << (bit_pos % 8)) != 0 {
            values.push(Value::Null);
            continue;
        }
        let (type_code, ..) = mysql_type_for(c.data_type);
        let v = match type_code {
            MYSQL_TYPE_TINY => Value::Bool(read_fixed(payload, &mut pos, 1).unwrap()[0] != 0),
            MYSQL_TYPE_LONG => {
                let b = read_fixed(payload, &mut pos, 4).unwrap();
                Value::Int32(i32::from_le_bytes(b.try_into().unwrap()))
            }
            MYSQL_TYPE_LONGLONG => {
                let b = read_fixed(payload, &mut pos, 8).unwrap();
                Value::Int64(i64::from_le_bytes(b.try_into().unwrap()))
            }
            MYSQL_TYPE_DOUBLE => {
                let b = read_fixed(payload, &mut pos, 8).unwrap();
                Value::Float64(f64::from_le_bytes(b.try_into().unwrap()))
            }
            MYSQL_TYPE_VAR_STRING => {
                let b = read_lenenc_str(payload, &mut pos).unwrap();
                Value::String(String::from_utf8(b.to_vec()).unwrap())
            }
            MYSQL_TYPE_BLOB => {
                let b = read_lenenc_str(payload, &mut pos).unwrap();
                Value::Bytes(b.to_vec())
            }
            other => panic!("decode_binary_row: unexpected type code 0x{other:02x}"),
        };
        values.push(v);
    }
    assert_eq!(pos, payload.len());
    Row::new(values)
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
    // `SET NAMES` and `SELECT @@sysvar` now flow through the connection's real
    // `htap_server::Session` (Phase 10 task 9), not a hardcoded shim.
    let r = ok(c.query("SET NAMES utf8mb4").unwrap());
    assert_eq!(r.affected_rows, 0);
    let rs = rows(c.query("select @@version_comment limit 1").unwrap());
    assert_eq!(rs, vec![Row::new(vec![Value::String("fluidb".into())])]);
    // A system variable's bind-time static column type is a placeholder nullable string (its
    // real type is only known when it's evaluated; see `htap_sql::expr::ExprType::is_dynamic`),
    // but the server infers the reported column type from the actual evaluated value
    // (storage-reviewer finding F10, `htap_server::query_exec::infer_dynamic_column_types`), so
    // an integer-valued variable like `max_allowed_packet` still decodes as `Int64`, exactly like
    // the old hardcoded shim. `@@socket`'s `NULL` is unaffected either way: `NULL` is
    // encoded/decoded the same way regardless of declared column type. `max_allowed_packet` is
    // now dynamic (Phase 11 plan task 8), reflecting this server's configured
    // `WireServerConfig::max_allowed_packet` default of 64 MiB (see
    // `test_max_allowed_packet_variable_reflects_wire_config` for a non-default value).
    let rs = rows(c.query("SELECT @@max_allowed_packet, @@socket").unwrap());
    assert_eq!(
        rs,
        vec![Row::new(vec![Value::Int64(64 * 1024 * 1024), Value::Null])]
    );
    let rs = rows(c.query("SELECT VERSION()").unwrap());
    assert_eq!(
        rs,
        vec![Row::new(vec![Value::String(SERVER_VERSION.into())])]
    );
    // Unknown system variables fail as SQL, now from the general query executor's bind step
    // rather than the shim.
    let err = c.query("SELECT @@does_not_exist").unwrap_err();
    assert!(matches!(err, WireError::Server { .. }));
    wire.shutdown();
}

#[test]
fn test_shim_charset_set_forms_over_wire() {
    // `SET CHARACTER SET <x>` / `SET CHARSET <x>` have no AST node in `vendor/sqlparser`, so
    // they are the one `SET` form still answered by the wire-layer shim rather than the
    // session (see `htap_wire::shim`).
    let (_dir, wire) = start(None, 4);
    let mut c = WireClient::connect(addr(&wire), None).unwrap();
    ok(c.query("SET CHARACTER SET utf8mb4").unwrap());
    ok(c.query("SET CHARSET utf8mb4").unwrap());
    // The connection is still usable afterwards, and other SET forms still work through the
    // real session.
    ok(c.query("SET autocommit = 1").unwrap());
    wire.shutdown();
}

// ---------------------------------------------------------------------------------------------
// Phase 10 task 9: one real `htap_server::Session` per connection.
// ---------------------------------------------------------------------------------------------

#[test]
fn test_wire_begin_commit_rollback_round_trip() {
    let (_dir, wire) = start(None, 4);
    let mut c = WireClient::connect(addr(&wire), None).unwrap();
    ok(c.query("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT)")
        .unwrap());

    ok(c.query("BEGIN").unwrap());
    ok(c.query("INSERT INTO t (id, v) VALUES (1, 10)").unwrap());
    // Read-your-own-writes on the same connection, before COMMIT.
    assert_eq!(
        rows(c.query("SELECT v FROM t WHERE id = 1").unwrap()),
        vec![Row::new(vec![Value::Int32(10)])]
    );

    // A second, separate connection (a separate session) does not see the uncommitted write.
    let mut other = WireClient::connect(addr(&wire), None).unwrap();
    assert!(rows(other.query("SELECT id FROM t WHERE id = 1").unwrap()).is_empty());

    ok(c.query("COMMIT").unwrap());
    assert_eq!(
        rows(other.query("SELECT v FROM t WHERE id = 1").unwrap()),
        vec![Row::new(vec![Value::Int32(10)])]
    );

    ok(c.query("BEGIN").unwrap());
    ok(c.query("UPDATE t SET v = 999 WHERE id = 1").unwrap());
    ok(c.query("ROLLBACK").unwrap());
    assert_eq!(
        rows(other.query("SELECT v FROM t WHERE id = 1").unwrap()),
        vec![Row::new(vec![Value::Int32(10)])],
        "a rolled-back transaction must never be visible"
    );
    wire.shutdown();
}

#[test]
fn test_wire_rollback_on_disconnect() {
    let (_dir, wire) = start(None, 4);
    let mut setup = WireClient::connect(addr(&wire), None).unwrap();
    ok(setup
        .query("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT)")
        .unwrap());

    {
        let mut c = WireClient::connect(addr(&wire), None).unwrap();
        ok(c.query("BEGIN").unwrap());
        ok(c.query("INSERT INTO t (id, v) VALUES (1, 10)").unwrap());
        c.quit().unwrap();
        // `c` disconnects without COMMIT: the wire layer's per-connection `Session` must roll
        // back its open transaction (explicit rollback on `COM_QUIT`, Phase 10 task 9).
    }

    assert!(
        rows(setup.query("SELECT id FROM t WHERE id = 1").unwrap()).is_empty(),
        "a disconnected connection's uncommitted write must never become visible"
    );
    wire.shutdown();
}

#[test]
fn test_wire_concurrent_sessions_conflict_returns_1213() {
    let (_dir, wire) = start(None, 4);
    let mut setup = WireClient::connect(addr(&wire), None).unwrap();
    ok(setup
        .query("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT)")
        .unwrap());
    ok(setup.query("INSERT INTO t (id, v) VALUES (1, 1)").unwrap());

    let mut a = WireClient::connect(addr(&wire), None).unwrap();
    ok(a.query("BEGIN").unwrap());
    ok(a.query("UPDATE t SET v = 2 WHERE id = 1").unwrap());

    // A second, autocommit connection commits the same key first.
    ok(setup.query("UPDATE t SET v = 99 WHERE id = 1").unwrap());

    let err = a.query("COMMIT").unwrap_err();
    assert_eq!(server_code(err), 1213);

    // The connection is still usable afterwards: the losing transaction is gone (aborted), not
    // stuck.
    assert_eq!(
        rows(a.query("SELECT v FROM t WHERE id = 1").unwrap()),
        vec![Row::new(vec![Value::Int32(99)])]
    );
    assert_eq!(
        rows(setup.query("SELECT v FROM t WHERE id = 1").unwrap()),
        vec![Row::new(vec![Value::Int32(99)])]
    );
    wire.shutdown();
}

#[test]
fn test_wire_set_autocommit_and_user_variable_round_trip() {
    let (_dir, wire) = start(None, 4);
    let mut c = WireClient::connect(addr(&wire), None).unwrap();
    ok(c.query("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT)")
        .unwrap());

    ok(c.query("SET @x = 42").unwrap());
    // `@x`'s bind-time static column type is a placeholder nullable string (see
    // `test_shim_set_and_version_comment`'s comment on `ExprType::is_dynamic` and
    // storage-reviewer finding F10), but the reported column type is inferred from the actual
    // value, so `@x` decodes as `Int64`, matching the session's own `Int64` storage.
    assert_eq!(
        rows(c.query("SELECT @x").unwrap()),
        vec![Row::new(vec![Value::Int64(42)])]
    );

    ok(c.query("SET autocommit = 0").unwrap());
    ok(c.query("INSERT INTO t (id, v) VALUES (1, 10)").unwrap());
    // Autocommit is off: the first statement implicitly began a transaction, so a separate
    // connection must not see the write yet.
    let mut other = WireClient::connect(addr(&wire), None).unwrap();
    assert!(rows(other.query("SELECT id FROM t WHERE id = 1").unwrap()).is_empty());

    ok(c.query("COMMIT").unwrap());
    assert_eq!(
        rows(other.query("SELECT v FROM t WHERE id = 1").unwrap()),
        vec![Row::new(vec![Value::Int32(10)])]
    );
    wire.shutdown();
}

#[test]
fn test_wire_sysvar_reads_now_reflect_session_state() {
    let (_dir, wire) = start(None, 4);
    let mut c = WireClient::connect(addr(&wire), None).unwrap();

    // `@@autocommit`'s bind-time static column type is a placeholder nullable string (see
    // `test_shim_set_and_version_comment`'s comment on `ExprType::is_dynamic` and
    // storage-reviewer finding F10), but the reported column type is inferred from the actual
    // value, so it decodes as `Int64`, matching `system_variable_value`'s own `Value::Int64`.
    //
    // Default: autocommit on.
    assert_eq!(
        rows(c.query("SELECT @@autocommit").unwrap()),
        vec![Row::new(vec![Value::Int64(1)])]
    );

    ok(c.query("SET autocommit = 0").unwrap());
    assert_eq!(
        rows(c.query("SELECT @@autocommit").unwrap()),
        vec![Row::new(vec![Value::Int64(0)])]
    );

    // A separate connection's session state is unaffected.
    let mut other = WireClient::connect(addr(&wire), None).unwrap();
    assert_eq!(
        rows(other.query("SELECT @@autocommit").unwrap()),
        vec![Row::new(vec![Value::Int64(1)])]
    );
    wire.shutdown();
}

/// The `mysql` crate (real driver) still connects and runs SQL under the new per-connection
/// session, including a `SET` form the shim still fakes (`SET NAMES`) and an ordinary
/// transaction.
#[test]
fn test_wire_mysql_connector_startup_still_works() {
    use mysql::prelude::*;
    let (_dir, wire) = start(None, 4);
    let opts = mysql::OptsBuilder::new()
        .ip_or_hostname(Some("127.0.0.1"))
        .tcp_port(addr(&wire).port())
        .user(Some("root"))
        .prefer_socket(false)
        .max_allowed_packet(Some(16 * 1024 * 1024));
    let mut conn = mysql::Conn::new(opts).unwrap();
    conn.ping().unwrap();
    conn.query_drop("SET NAMES utf8mb4").unwrap();
    conn.query_drop("CREATE TABLE conn_t (id BIGINT PRIMARY KEY, v INT)")
        .unwrap();
    conn.query_drop("START TRANSACTION").unwrap();
    conn.query_drop("INSERT INTO conn_t (id, v) VALUES (1, 10)")
        .unwrap();
    conn.query_drop("COMMIT").unwrap();
    let got: Option<i32> = conn
        .query_first("SELECT v FROM conn_t WHERE id = 1")
        .unwrap();
    assert_eq!(got, Some(10));
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

// ---------------------------------------------------------------------------------------------
// Phase 11 plan task 4: COM_STMT_PREPARE / EXECUTE / CLOSE / RESET / SEND_LONG_DATA / FETCH.
// ---------------------------------------------------------------------------------------------

/// The `mysql` crate (real driver) prepares statements, binds every engine type (including NULL,
/// a `Vec<u8>` with non-UTF-8 bytes, and a `mysql::Value::Date` timestamp), executes them via
/// `exec_drop`/`exec`/`exec_iter`, and reuses the same prepared `Statement` across several
/// executes; binary SELECT results are cross-checked against the text protocol's own `query()`.
#[test]
fn test_prepared_statements_mysql_crate_interop_all_types() {
    use mysql::prelude::*;
    use mysql::{Params, Value as MyValue};

    let (_dir, wire) = start(None, 4);
    let opts = mysql::OptsBuilder::new()
        .ip_or_hostname(Some("127.0.0.1"))
        .tcp_port(addr(&wire).port())
        .user(Some("root"))
        .prefer_socket(false)
        .max_allowed_packet(Some(16 * 1024 * 1024));
    let mut conn = mysql::Conn::new(opts).unwrap();

    conn.query_drop(
        "CREATE TABLE ps (id INT PRIMARY KEY, flag BOOL, big BIGINT, amt DOUBLE, \
         name VARCHAR(32), raw VARBINARY(16), ts TIMESTAMP)",
    )
    .unwrap();

    let insert = conn
        .prep("INSERT INTO ps (id, flag, big, amt, name, raw, ts) VALUES (?, ?, ?, ?, ?, ?, ?)")
        .unwrap();
    let non_utf8 = vec![0x66u8, 0x6f, 0xff, 0x6f];

    // Row 1: every type populated, including a `Vec<u8>` that is not valid UTF-8 and an explicit
    // `mysql::Value::Date`.
    conn.exec_drop(
        &insert,
        Params::Positional(vec![
            MyValue::Int(1),
            MyValue::Int(1), // TINY 1 -> BOOL true (amendment A3)
            MyValue::Int(1_000_000_000_000),
            MyValue::Double(2.5),
            MyValue::Bytes(b"ann".to_vec()),
            MyValue::Bytes(non_utf8.clone()),
            MyValue::Date(2023, 11, 14, 22, 13, 20, 123_456),
        ]),
    )
    .unwrap();
    assert_eq!(conn.affected_rows(), 1);

    // Row 2: every column NULL.
    conn.exec_drop(
        &insert,
        Params::Positional(vec![
            MyValue::Int(2),
            MyValue::NULL,
            MyValue::NULL,
            MyValue::NULL,
            MyValue::NULL,
            MyValue::NULL,
            MyValue::NULL,
        ]),
    )
    .unwrap();

    // Row 3: statement reuse (same `Statement` handle, third `exec_drop`) with TINY 0 -> BOOL
    // false and a `bool` Rust value bound directly.
    conn.exec_drop(
        &insert,
        Params::Positional(vec![
            MyValue::Int(3),
            MyValue::from(false),
            MyValue::Int(0),
            MyValue::Double(0.0),
            MyValue::Bytes(b"cid".to_vec()),
            MyValue::Bytes(vec![]),
            MyValue::Date(2000, 1, 1, 0, 0, 0, 0),
        ]),
    )
    .unwrap();

    // SELECT with a bound parameter (binary protocol) vs. the equivalent literal SQL (text
    // protocol): every non-BLOB/non-DATETIME column converts identically through `FromValue`
    // regardless of which protocol produced it.
    let select = conn
        .prep("SELECT id, flag, big, amt, name, raw, ts FROM ps WHERE id = ?")
        .unwrap();
    let bin_row: mysql::Row = conn.exec_first(&select, (1,)).unwrap().unwrap();
    let text_row: mysql::Row = conn
        .query_first("SELECT id, flag, big, amt, name, raw, ts FROM ps WHERE id = 1")
        .unwrap()
        .unwrap();
    assert_eq!(
        bin_row.get::<i32, _>(0).unwrap(),
        text_row.get::<i32, _>(0).unwrap()
    );
    assert_eq!(bin_row.get::<i32, _>(0).unwrap(), 1);
    assert_eq!(
        bin_row.get::<bool, _>(1).unwrap(),
        text_row.get::<bool, _>(1).unwrap()
    );
    assert!(bin_row.get::<bool, _>(1).unwrap());
    assert_eq!(
        bin_row.get::<i64, _>(2).unwrap(),
        text_row.get::<i64, _>(2).unwrap()
    );
    assert_eq!(
        bin_row.get::<f64, _>(3).unwrap(),
        text_row.get::<f64, _>(3).unwrap()
    );
    assert_eq!(
        bin_row.get::<Vec<u8>, _>(4).unwrap(),
        text_row.get::<Vec<u8>, _>(4).unwrap()
    );
    assert_eq!(bin_row.get::<Vec<u8>, _>(5).unwrap(), non_utf8);
    assert_eq!(text_row.get::<Vec<u8>, _>(5).unwrap(), non_utf8);
    // The binary protocol decodes DATETIME as a typed `Value::Date`; the text protocol always
    // decodes it as `Value::Bytes` (the date string) — see `mysql_common::Value::deserialize_text`
    // vs `deserialize_bin`. Check both independently against the bound value.
    assert_eq!(
        bin_row.get::<mysql::Value, _>(6).unwrap(),
        mysql::Value::Date(2023, 11, 14, 22, 13, 20, 123_456)
    );
    assert_eq!(
        String::from_utf8(text_row.get::<Vec<u8>, _>(6).unwrap()).unwrap(),
        "2023-11-14 22:13:20.123456"
    );

    // NULL row round-trips through the binary protocol (column 0 is the non-NULL primary key).
    let null_row: mysql::Row = conn.exec_first(&select, (2,)).unwrap().unwrap();
    assert_eq!(null_row.get::<i32, _>(0).unwrap(), 2);
    for i in 1..7 {
        assert_eq!(
            null_row.get::<mysql::Value, _>(i).unwrap(),
            mysql::Value::NULL
        );
    }

    // UPDATE and DELETE with bound parameters, and `exec_iter`.
    let update = conn.prep("UPDATE ps SET amt = ? WHERE id = ?").unwrap();
    conn.exec_drop(&update, (9.5f64, 3)).unwrap();
    assert_eq!(conn.affected_rows(), 1);
    let amt_select = conn.prep("SELECT amt FROM ps WHERE id = ?").unwrap();
    let got: f64 = conn.exec_first(&amt_select, (3,)).unwrap().unwrap();
    assert_eq!(got, 9.5);

    let count_stmt = conn.prep("SELECT id FROM ps WHERE id >= ?").unwrap();
    let mut ids: Vec<i32> = Vec::new();
    for row in conn.exec_iter(&count_stmt, (1,)).unwrap() {
        let row = row.unwrap();
        ids.push(row.get(0).unwrap());
    }
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2, 3]);

    let delete = conn.prep("DELETE FROM ps WHERE id = ?").unwrap();
    conn.exec_drop(&delete, (2,)).unwrap();
    assert_eq!(conn.affected_rows(), 1);
    let remaining: Option<i32> = conn.exec_first(&select, (2,)).unwrap();
    assert_eq!(remaining, None);

    wire.shutdown();
}

/// Amendment A2: a connector that sets `new_params_bound_flag = 0` on a re-execute (unchanged
/// parameter types) must have that execute decoded using the cached types from the last execute
/// that set the flag to 1; a statement that has never had its types cached rejects `flag = 0`
/// cleanly instead of guessing.
#[test]
fn test_prepared_statement_param_type_cache_new_params_bound_zero() {
    let (_dir, wire) = start(None, 4);
    let mut setup = WireClient::connect(addr(&wire), None).unwrap();
    ok(setup
        .query("CREATE TABLE tc (id BIGINT PRIMARY KEY, v INT)")
        .unwrap());
    ok(setup.query("INSERT INTO tc (id, v) VALUES (1, 0)").unwrap());

    let mut stream = TcpStream::connect(addr(&wire)).unwrap();
    raw_handshake(&mut stream);

    let (prepare_ok, _) = raw_prepare(&mut stream, "UPDATE tc SET v = ? WHERE id = 1");
    assert_eq!(prepare_ok.num_params, 1);
    assert_eq!(prepare_ok.num_columns, 0);

    // First EXECUTE: flag = 1, sends the type.
    let p1 = build_execute_payload(
        prepare_ok.stmt_id,
        &[(MYSQL_TYPE_LONG, false, Some(42i32.to_le_bytes().to_vec()))],
        true,
    );
    let resp1 = raw_execute(&mut stream, &p1);
    assert_eq!(resp1[0], OK_HEADER, "{resp1:?}");
    assert_eq!(
        rows(setup.query("SELECT v FROM tc WHERE id = 1").unwrap()),
        vec![Row::new(vec![Value::Int32(42)])]
    );

    // Second EXECUTE: flag = 0, no type bytes at all -> must reuse the cached LONG type.
    let mut p2 = Vec::new();
    p2.extend_from_slice(&prepare_ok.stmt_id.to_le_bytes());
    p2.push(CURSOR_TYPE_NO_CURSOR);
    p2.extend_from_slice(&1u32.to_le_bytes());
    p2.push(0); // bitmap: not null
    p2.push(0); // new_params_bound_flag = 0
    p2.extend_from_slice(&99i32.to_le_bytes());
    let resp2 = raw_execute(&mut stream, &p2);
    assert_eq!(resp2[0], OK_HEADER, "{resp2:?}");
    assert_eq!(
        rows(setup.query("SELECT v FROM tc WHERE id = 1").unwrap()),
        vec![Row::new(vec![Value::Int32(99)])]
    );

    // A statement that has never been executed with flag = 1 rejects flag = 0 cleanly, and the
    // connection stays usable afterward.
    let (prepare_ok2, _) = raw_prepare(&mut stream, "UPDATE tc SET v = ? WHERE id = 1");
    let mut p3 = Vec::new();
    p3.extend_from_slice(&prepare_ok2.stmt_id.to_le_bytes());
    p3.push(CURSOR_TYPE_NO_CURSOR);
    p3.extend_from_slice(&1u32.to_le_bytes());
    p3.push(0);
    p3.push(0); // flag = 0, no cache yet
    p3.extend_from_slice(&1i32.to_le_bytes());
    let resp3 = raw_execute(&mut stream, &p3);
    assert_eq!(resp3[0], ERR_HEADER);
    assert!(setup.query("SELECT 1").is_ok());

    wire.shutdown();
}

#[test]
fn test_prepared_statement_unknown_id_and_close_and_reset() {
    let (_dir, wire) = start(None, 4);
    let mut stream = TcpStream::connect(addr(&wire)).unwrap();
    raw_handshake(&mut stream);

    // EXECUTE against an id this connection never prepared -> 1243.
    let payload = build_execute_payload(999, &[], true);
    let resp = raw_execute(&mut stream, &payload);
    assert_eq!(resp[0], ERR_HEADER);
    let (code, ..) = htap_wire::error_map::parse_err_payload(&resp).unwrap();
    assert_eq!(code, 1243);

    // RESET against an unknown id -> 1243 too.
    let reset_body = 999u32.to_le_bytes().to_vec();
    raw_send_command(&mut stream, COM_STMT_RESET, &reset_body);
    let (_, resp) = read_packet(&mut stream).unwrap();
    assert_eq!(resp[0], ERR_HEADER);
    let (code, ..) = htap_wire::error_map::parse_err_payload(&resp).unwrap();
    assert_eq!(code, 1243);

    // Prepare a real statement, close it (no response), then EXECUTE against it -> 1243. The
    // connection stays fully usable throughout (PING succeeds before and after).
    let (prepare_ok, _) = raw_prepare(&mut stream, "SELECT 1");
    raw_send_command(
        &mut stream,
        COM_STMT_CLOSE,
        &prepare_ok.stmt_id.to_le_bytes(),
    );
    raw_send_command(&mut stream, COM_PING, &[]);
    let (_, pong) = read_packet(&mut stream).unwrap();
    assert_eq!(pong[0], OK_HEADER);

    let payload = build_execute_payload(prepare_ok.stmt_id, &[], true);
    let resp = raw_execute(&mut stream, &payload);
    assert_eq!(resp[0], ERR_HEADER);
    let (code, ..) = htap_wire::error_map::parse_err_payload(&resp).unwrap();
    assert_eq!(code, 1243);

    // RESET on a real, still-open statement succeeds with OK.
    let (prepare_ok2, _) = raw_prepare(&mut stream, "SELECT 1");
    raw_send_command(
        &mut stream,
        COM_STMT_RESET,
        &prepare_ok2.stmt_id.to_le_bytes(),
    );
    let (_, resp) = read_packet(&mut stream).unwrap();
    assert_eq!(resp[0], OK_HEADER);

    wire.shutdown();
}

/// `COM_STMT_SEND_LONG_DATA` accumulates bytes across multiple packets with no response of its
/// own, `EXECUTE` consumes and clears them, and an error (an out-of-range parameter index, in
/// place of the 64 MiB default byte cap — see this test's doc comment) poisons the statement so
/// it surfaces cleanly at the next `EXECUTE` rather than being silently dropped; `COM_STMT_RESET`
/// clears the poison.
///
/// The byte cap itself (`PreparedStatementRegistry::append_long_data`'s
/// `max_long_data_bytes` check) is covered by `htap_wire::prepared`'s own unit tests
/// (`append_long_data_poisons_over_the_byte_cap`) instead of here: this connection's cap is the
/// Phase 11 plan task 8 placeholder default of 64 MiB, and there is no server-visible knob yet to
/// shrink it for a fast integration test (task 8 adds `--max-allowed-packet`); sending 64+ MiB
/// over a real socket in this test would be needlessly slow.
#[test]
fn test_prepared_statement_send_long_data() {
    let (_dir, wire) = start(None, 4);
    let mut setup = WireClient::connect(addr(&wire), None).unwrap();
    ok(setup
        .query("CREATE TABLE ld (id INT PRIMARY KEY, blob VARBINARY(64))")
        .unwrap());

    let mut stream = TcpStream::connect(addr(&wire)).unwrap();
    raw_handshake(&mut stream);
    let (prepare_ok, _) = raw_prepare(&mut stream, "INSERT INTO ld (id, blob) VALUES (?, ?)");
    assert_eq!(prepare_ok.num_params, 2);

    let mut chunk1 = prepare_ok.stmt_id.to_le_bytes().to_vec();
    chunk1.extend_from_slice(&1u16.to_le_bytes());
    chunk1.extend_from_slice(b"hello ");
    raw_send_command(&mut stream, COM_STMT_SEND_LONG_DATA, &chunk1);
    let mut chunk2 = prepare_ok.stmt_id.to_le_bytes().to_vec();
    chunk2.extend_from_slice(&1u16.to_le_bytes());
    chunk2.extend_from_slice(b"world");
    raw_send_command(&mut stream, COM_STMT_SEND_LONG_DATA, &chunk2);

    // EXECUTE: param 0 (id) is an ordinary value; param 1 (blob) is long data, so its type is
    // sent but no value bytes follow for it.
    let mut payload = Vec::new();
    payload.extend_from_slice(&prepare_ok.stmt_id.to_le_bytes());
    payload.push(CURSOR_TYPE_NO_CURSOR);
    payload.extend_from_slice(&1u32.to_le_bytes());
    payload.push(0); // bitmap: neither null
    payload.push(1); // new_params_bound_flag
    payload.push(MYSQL_TYPE_LONG);
    payload.push(0);
    payload.push(MYSQL_TYPE_BLOB);
    payload.push(0);
    payload.extend_from_slice(&7i32.to_le_bytes()); // only param 0's value bytes
    let resp = raw_execute(&mut stream, &payload);
    assert_eq!(resp[0], OK_HEADER, "{resp:?}");
    assert_eq!(
        rows(setup.query("SELECT blob FROM ld WHERE id = 7").unwrap()),
        vec![Row::new(vec![Value::Bytes(b"hello world".to_vec())])]
    );

    // An out-of-range parameter index poisons the statement; SEND_LONG_DATA itself has no
    // response, but the next EXECUTE surfaces the stored error cleanly.
    let (prepare_ok2, _) = raw_prepare(&mut stream, "INSERT INTO ld (id, blob) VALUES (?, ?)");
    let mut bad_chunk = prepare_ok2.stmt_id.to_le_bytes().to_vec();
    bad_chunk.extend_from_slice(&5u16.to_le_bytes()); // out of range: only 2 params
    bad_chunk.extend_from_slice(b"oops");
    raw_send_command(&mut stream, COM_STMT_SEND_LONG_DATA, &bad_chunk);
    let payload2 = build_execute_payload(
        prepare_ok2.stmt_id,
        &[
            (MYSQL_TYPE_LONG, false, Some(1i32.to_le_bytes().to_vec())),
            (MYSQL_TYPE_BLOB, false, Some(lenenc_bytes(&[]))),
        ],
        true,
    );
    let resp2 = raw_execute(&mut stream, &payload2);
    assert_eq!(resp2[0], ERR_HEADER);
    let (_, _, msg) = htap_wire::error_map::parse_err_payload(&resp2).unwrap();
    assert!(msg.contains("out of range"), "{msg}");

    // COM_STMT_RESET clears the poison: a fresh EXECUTE succeeds again.
    raw_send_command(
        &mut stream,
        COM_STMT_RESET,
        &prepare_ok2.stmt_id.to_le_bytes(),
    );
    let (_, resp) = read_packet(&mut stream).unwrap();
    assert_eq!(resp[0], OK_HEADER);
    let payload3 = build_execute_payload(
        prepare_ok2.stmt_id,
        &[
            (MYSQL_TYPE_LONG, false, Some(2i32.to_le_bytes().to_vec())),
            (MYSQL_TYPE_BLOB, false, Some(lenenc_bytes(b"ok"))),
        ],
        true,
    );
    let resp3 = raw_execute(&mut stream, &payload3);
    assert_eq!(resp3[0], OK_HEADER, "{resp3:?}");

    wire.shutdown();
}

/// `COM_STMT_EXECUTE` goes through `Session::execute_statement`, so it participates in an
/// explicit transaction exactly like a text-protocol statement and observes the same
/// `CommitOutcomePending` quarantine after an ambiguous commit.
#[test]
fn test_prepared_statement_in_transaction_and_commit_outcome_pending() {
    let (_dir, server, wire) = start_with_server(None, 4);
    let mut setup = WireClient::connect(addr(&wire), None).unwrap();
    ok(setup
        .query("CREATE TABLE cp (id BIGINT PRIMARY KEY, v INT)")
        .unwrap());

    let mut stream = TcpStream::connect(addr(&wire)).unwrap();
    raw_handshake(&mut stream);

    raw_send_command(&mut stream, COM_QUERY, b"BEGIN");
    let (_, resp) = read_packet(&mut stream).unwrap();
    assert_eq!(resp[0], OK_HEADER);

    let (prepare_ok, _) = raw_prepare(&mut stream, "INSERT INTO cp (id, v) VALUES (?, ?)");
    let payload = build_execute_payload(
        prepare_ok.stmt_id,
        &[
            (
                MYSQL_TYPE_LONGLONG,
                false,
                Some(1i64.to_le_bytes().to_vec()),
            ),
            (MYSQL_TYPE_LONG, false, Some(10i32.to_le_bytes().to_vec())),
        ],
        true,
    );
    let resp = raw_execute(&mut stream, &payload);
    assert_eq!(resp[0], OK_HEADER, "{resp:?}");

    server.txn_manager().set_commit_append_hook(|_journal| {
        Err(HtapError::Io(std::io::Error::other(
            "simulated disk failure during commit record append",
        )))
    });
    raw_send_command(&mut stream, COM_QUERY, b"COMMIT");
    let (_, resp) = read_packet(&mut stream).unwrap();
    assert_eq!(resp[0], ERR_HEADER);
    let (code, ..) = htap_wire::error_map::parse_err_payload(&resp).unwrap();
    assert_eq!(code, 1105, "DurablePending maps to ER_UNKNOWN, never 1213");

    // The session is now quarantined: a further EXECUTE of the same prepared statement returns
    // the same stored `DurablePending` error, never a fresh attempt.
    let payload2 = build_execute_payload(
        prepare_ok.stmt_id,
        &[
            (
                MYSQL_TYPE_LONGLONG,
                false,
                Some(2i64.to_le_bytes().to_vec()),
            ),
            (MYSQL_TYPE_LONG, false, Some(20i32.to_le_bytes().to_vec())),
        ],
        true,
    );
    let resp2 = raw_execute(&mut stream, &payload2);
    assert_eq!(resp2[0], ERR_HEADER);
    let (code2, ..) = htap_wire::error_map::parse_err_payload(&resp2).unwrap();
    assert_eq!(code2, 1105);

    wire.shutdown();
}

#[test]
fn test_prepared_statement_unsupported_kinds_rejected() {
    let (_dir, wire) = start(None, 4);
    let mut stream = TcpStream::connect(addr(&wire)).unwrap();
    raw_handshake(&mut stream);
    for sql in [
        "SET autocommit = 1",
        "BEGIN",
        "COMMIT",
        "CREATE TABLE x (id INT PRIMARY KEY)",
        "SHOW TABLES",
    ] {
        let err = raw_prepare_result(&mut stream, sql)
            .err()
            .unwrap_or_else(|| panic!("{sql} should have been rejected"));
        assert_eq!(err.0, 1235, "{sql}: {err:?}");
    }
    // Connection still usable afterward.
    raw_send_command(&mut stream, COM_PING, &[]);
    let (_, pong) = read_packet(&mut stream).unwrap();
    assert_eq!(pong[0], OK_HEADER);
    wire.shutdown();
}

#[test]
fn test_prepare_placeholder_in_limit_and_subquery() {
    let (_dir, wire) = start(None, 4);
    let mut setup = WireClient::connect(addr(&wire), None).unwrap();
    ok(setup
        .query("CREATE TABLE pl (id INT PRIMARY KEY, v INT)")
        .unwrap());
    ok(setup
        .query("INSERT INTO pl (id, v) VALUES (1, 10), (2, 20), (3, 30)")
        .unwrap());

    let mut stream = TcpStream::connect(addr(&wire)).unwrap();
    raw_handshake(&mut stream);

    let (prepare_ok, _) = raw_prepare(&mut stream, "SELECT id FROM pl ORDER BY id LIMIT ?");
    assert_eq!(prepare_ok.num_params, 1);
    let payload = build_execute_payload(
        prepare_ok.stmt_id,
        &[(MYSQL_TYPE_LONG, false, Some(2i32.to_le_bytes().to_vec()))],
        true,
    );
    let (columns, row_payloads) = raw_execute_query(&mut stream, &payload);
    let got: Vec<Row> = row_payloads
        .iter()
        .map(|p| decode_binary_row(p, &columns))
        .collect();
    assert_eq!(
        got,
        vec![
            Row::new(vec![Value::Int32(1)]),
            Row::new(vec![Value::Int32(2)])
        ]
    );

    let (prepare_ok2, _) = raw_prepare(
        &mut stream,
        "SELECT id FROM pl WHERE id IN (SELECT id FROM pl WHERE v > ?) ORDER BY id",
    );
    assert_eq!(prepare_ok2.num_params, 1);
    let payload2 = build_execute_payload(
        prepare_ok2.stmt_id,
        &[(MYSQL_TYPE_LONG, false, Some(10i32.to_le_bytes().to_vec()))],
        true,
    );
    let (columns2, row_payloads2) = raw_execute_query(&mut stream, &payload2);
    let got2: Vec<Row> = row_payloads2
        .iter()
        .map(|p| decode_binary_row(p, &columns2))
        .collect();
    assert_eq!(
        got2,
        vec![
            Row::new(vec![Value::Int32(2)]),
            Row::new(vec![Value::Int32(3)])
        ]
    );

    wire.shutdown();
}

// ---------------------------------------------------------------------------------------------
// Fix pass (finding 5): a placeholder nested inside a CASE in ORDER BY must PREPARE and EXECUTE
// correctly over the wire, not just at the htap-sql unit level.
// ---------------------------------------------------------------------------------------------

#[test]
fn test_wire_prepare_and_execute_case_in_order_by() {
    let (_dir, wire) = start(None, 4);
    let mut setup = WireClient::connect(addr(&wire), None).unwrap();
    ok(setup
        .query("CREATE TABLE cob (id INT PRIMARY KEY, n INT)")
        .unwrap());
    ok(setup
        .query("INSERT INTO cob (id, n) VALUES (1, 5), (2, 1), (3, 9)")
        .unwrap());

    let mut stream = TcpStream::connect(addr(&wire)).unwrap();
    raw_handshake(&mut stream);
    let (prepare_ok, _) = raw_prepare(
        &mut stream,
        "SELECT id FROM cob ORDER BY CASE WHEN ? = 1 THEN id ELSE -id END",
    );
    assert_eq!(prepare_ok.num_params, 1);

    // `? = 1` -> ascending id order.
    let payload = build_execute_payload(
        prepare_ok.stmt_id,
        &[(MYSQL_TYPE_LONG, false, Some(1i32.to_le_bytes().to_vec()))],
        true,
    );
    let (columns, row_payloads) = raw_execute_query(&mut stream, &payload);
    let got: Vec<Row> = row_payloads
        .iter()
        .map(|p| decode_binary_row(p, &columns))
        .collect();
    assert_eq!(
        got,
        vec![
            Row::new(vec![Value::Int32(1)]),
            Row::new(vec![Value::Int32(2)]),
            Row::new(vec![Value::Int32(3)]),
        ]
    );

    // `? = 0` (anything else) -> descending id order.
    let payload = build_execute_payload(
        prepare_ok.stmt_id,
        &[(MYSQL_TYPE_LONG, false, Some(0i32.to_le_bytes().to_vec()))],
        true,
    );
    let (columns, row_payloads) = raw_execute_query(&mut stream, &payload);
    let got: Vec<Row> = row_payloads
        .iter()
        .map(|p| decode_binary_row(p, &columns))
        .collect();
    assert_eq!(
        got,
        vec![
            Row::new(vec![Value::Int32(3)]),
            Row::new(vec![Value::Int32(2)]),
            Row::new(vec![Value::Int32(1)]),
        ]
    );

    wire.shutdown();
}

// ---------------------------------------------------------------------------------------------
// Fix pass (finding 2): DECIMAL/NEWDECIMAL parameters substitute exactly, with no f64 round trip.
// ---------------------------------------------------------------------------------------------

/// A `NEWDECIMAL` prepared-statement parameter whose text value is one past `2^53` (the largest
/// integer an `f64` can represent exactly) round-trips into a `BIGINT` column exactly, and a
/// literal SQL `INSERT` of the same text produces the same stored value.
#[test]
fn test_wire_prepared_decimal_param_round_trips_exactly_into_bigint_column() {
    let (_dir, wire) = start(None, 4);
    let text = "9007199254740993"; // 2^53 + 1
    assert_ne!(
        text.parse::<f64>().unwrap() as i64,
        text.parse::<i64>().unwrap(),
        "sanity check: this value must actually lose precision through f64"
    );

    let mut stream = TcpStream::connect(addr(&wire)).unwrap();
    raw_handshake(&mut stream);
    raw_send_command(
        &mut stream,
        COM_QUERY,
        b"CREATE TABLE dec_t (id BIGINT PRIMARY KEY, v BIGINT)",
    );
    let (_, resp) = read_packet(&mut stream).unwrap();
    assert_eq!(resp[0], OK_HEADER, "{resp:?}");

    let (prepare_ok, _) = raw_prepare(&mut stream, "INSERT INTO dec_t (id, v) VALUES (?, ?)");
    let payload = build_execute_payload(
        prepare_ok.stmt_id,
        &[
            (
                MYSQL_TYPE_LONGLONG,
                false,
                Some(1i64.to_le_bytes().to_vec()),
            ),
            (
                MYSQL_TYPE_NEWDECIMAL,
                false,
                Some(lenenc_bytes(text.as_bytes())),
            ),
        ],
        true,
    );
    let resp = raw_execute(&mut stream, &payload);
    assert_eq!(
        resp[0], OK_HEADER,
        "prepared DECIMAL insert failed: {resp:?}"
    );

    // Literal SQL: the same text, typed directly, must bind identically (same column, same
    // precision expectation).
    raw_send_command(
        &mut stream,
        COM_QUERY,
        format!("INSERT INTO dec_t (id, v) VALUES (2, {text})").as_bytes(),
    );
    let (_, resp) = read_packet(&mut stream).unwrap();
    assert_eq!(
        resp[0], OK_HEADER,
        "literal DECIMAL insert failed: {resp:?}"
    );

    let mut c = WireClient::connect(addr(&wire), None).unwrap();
    let expected = vec![Row::new(vec![Value::Int64(text.parse::<i64>().unwrap())])];
    assert_eq!(
        rows(c.query("SELECT v FROM dec_t WHERE id = 1").unwrap()),
        expected,
        "prepared DECIMAL parameter must round-trip exactly"
    );
    assert_eq!(
        rows(c.query("SELECT v FROM dec_t WHERE id = 2").unwrap()),
        expected,
        "literal DECIMAL SQL must bind identically to the prepared parameter"
    );

    wire.shutdown();
}

// ---------------------------------------------------------------------------------------------
// Phase 11 plan task 6: COM_RESET_CONNECTION.
// ---------------------------------------------------------------------------------------------

#[test]
fn test_wire_reset_connection_clears_state_and_prepared_statements() {
    let (_dir, wire) = start(None, 4);
    let mut stream = TcpStream::connect(addr(&wire)).unwrap();
    raw_handshake(&mut stream);

    raw_send_command(&mut stream, COM_QUERY, b"SET @x = 42");
    let (_, resp) = read_packet(&mut stream).unwrap();
    assert_eq!(resp[0], OK_HEADER);

    let (prepare_ok, _) = raw_prepare(&mut stream, "SELECT 1");

    raw_send_command(&mut stream, COM_RESET_CONNECTION, &[]);
    let (_, resp) = read_packet(&mut stream).unwrap();
    assert_eq!(resp[0], OK_HEADER);

    // `@x` is gone (session state cleared).
    raw_send_command(&mut stream, COM_QUERY, b"SELECT @x");
    let (_, count) = read_packet(&mut stream).unwrap();
    assert_eq!(count, vec![1]);
    let (_, _def) = read_packet(&mut stream).unwrap();
    let (_, row) = read_packet(&mut stream).unwrap();
    assert_eq!(row, vec![NULL_MARKER]);
    let (_, term) = read_packet(&mut stream).unwrap();
    assert!(is_resultset_terminator(&term));

    // The prepared statement is gone too: EXECUTE against it now returns 1243.
    let payload = build_execute_payload(prepare_ok.stmt_id, &[], true);
    let resp = raw_execute(&mut stream, &payload);
    assert_eq!(resp[0], ERR_HEADER);
    let (code, ..) = htap_wire::error_map::parse_err_payload(&resp).unwrap();
    assert_eq!(code, 1243);

    wire.shutdown();
}

#[test]
fn test_wire_reset_connection_while_commit_outcome_pending_stays_quarantined() {
    let (_dir, server, wire) = start_with_server(None, 4);
    let mut setup = WireClient::connect(addr(&wire), None).unwrap();
    ok(setup
        .query("CREATE TABLE rq (id BIGINT PRIMARY KEY, v INT)")
        .unwrap());

    let mut stream = TcpStream::connect(addr(&wire)).unwrap();
    raw_handshake(&mut stream);
    raw_send_command(&mut stream, COM_QUERY, b"BEGIN");
    read_packet(&mut stream).unwrap();
    raw_send_command(
        &mut stream,
        COM_QUERY,
        b"INSERT INTO rq (id, v) VALUES (1, 1)",
    );
    read_packet(&mut stream).unwrap();

    server.txn_manager().set_commit_append_hook(|_journal| {
        Err(HtapError::Io(std::io::Error::other(
            "simulated disk failure",
        )))
    });
    raw_send_command(&mut stream, COM_QUERY, b"COMMIT");
    let (_, resp) = read_packet(&mut stream).unwrap();
    assert_eq!(resp[0], ERR_HEADER);

    // COM_RESET_CONNECTION reports the same quarantine error and changes nothing.
    raw_send_command(&mut stream, COM_RESET_CONNECTION, &[]);
    let (_, resp) = read_packet(&mut stream).unwrap();
    assert_eq!(resp[0], ERR_HEADER);
    let (code, ..) = htap_wire::error_map::parse_err_payload(&resp).unwrap();
    assert_eq!(code, 1105);

    // COM_PING is never gated: the connection stays alive.
    raw_send_command(&mut stream, COM_PING, &[]);
    let (_, pong) = read_packet(&mut stream).unwrap();
    assert_eq!(pong[0], OK_HEADER);

    wire.shutdown();
}

#[test]
fn test_wire_quit_allowed_while_commit_outcome_pending() {
    use std::io::Read;

    let (_dir, server, wire) = start_with_server(None, 4);
    let mut setup = WireClient::connect(addr(&wire), None).unwrap();
    ok(setup
        .query("CREATE TABLE qq (id BIGINT PRIMARY KEY, v INT)")
        .unwrap());

    let mut stream = TcpStream::connect(addr(&wire)).unwrap();
    raw_handshake(&mut stream);
    raw_send_command(&mut stream, COM_QUERY, b"BEGIN");
    read_packet(&mut stream).unwrap();
    raw_send_command(
        &mut stream,
        COM_QUERY,
        b"INSERT INTO qq (id, v) VALUES (1, 1)",
    );
    read_packet(&mut stream).unwrap();

    server.txn_manager().set_commit_append_hook(|_journal| {
        Err(HtapError::Io(std::io::Error::other(
            "simulated disk failure",
        )))
    });
    raw_send_command(&mut stream, COM_QUERY, b"COMMIT");
    let (_, resp) = read_packet(&mut stream).unwrap();
    assert_eq!(resp[0], ERR_HEADER);

    // COM_QUIT is accepted and closes the connection cleanly, never routed through the
    // quarantine gate (which would otherwise answer it with an ERR packet instead of closing).
    raw_send_command(&mut stream, COM_QUIT, &[]);
    let mut buf = [0u8; 1];
    let n = stream.read(&mut buf);
    assert!(
        matches!(n, Ok(0)) || n.is_err(),
        "server must close the connection on QUIT, got {n:?}"
    );

    wire.shutdown();
}

// ---------------------------------------------------------------------------------------------
// Phase 11 plan task 7: COM_CHANGE_USER.
// ---------------------------------------------------------------------------------------------

#[test]
fn test_wire_change_user_reauth_and_reset() {
    use mysql::prelude::*;
    let (_dir, wire) = start(Some("secret"), 4);
    let opts = mysql::OptsBuilder::new()
        .ip_or_hostname(Some("127.0.0.1"))
        .tcp_port(addr(&wire).port())
        .user(Some("root"))
        .pass(Some("secret"))
        .prefer_socket(false)
        .max_allowed_packet(Some(16 * 1024 * 1024));
    let mut conn = mysql::Conn::new(opts).unwrap();
    conn.query_drop("SET autocommit = 0").unwrap();
    let got: i64 = conn.query_first("SELECT @@autocommit").unwrap().unwrap();
    assert_eq!(got, 0);

    conn.change_user(mysql::ChangeUserOpts::DEFAULT).unwrap();

    // Session state reset: autocommit is back to its default (on).
    let got: i64 = conn.query_first("SELECT @@autocommit").unwrap().unwrap();
    assert_eq!(got, 1);

    // The connection is still fully usable afterward.
    conn.query_drop("CREATE TABLE cu (id INT PRIMARY KEY)")
        .unwrap();
    conn.query_drop("INSERT INTO cu (id) VALUES (1)").unwrap();
    let got: Option<i32> = conn.query_first("SELECT id FROM cu WHERE id = 1").unwrap();
    assert_eq!(got, Some(1));
    drop(conn);

    // Raw-protocol check that a successful COM_CHANGE_USER also clears the prepared-statement
    // registry: real clients (confirmed against `mysql-28.0.2`'s `exec_com_change_user`) hash
    // COM_CHANGE_USER's auth response against the connection's *original* handshake scramble,
    // never a fresh one (see `htap_wire::server`'s `Session::scramble` doc comment).
    let mut stream = TcpStream::connect(addr(&wire)).unwrap();
    let scramble = raw_handshake_with_password(&mut stream, b"secret");
    let (prepare_ok, _) = raw_prepare(&mut stream, "SELECT 1");

    let mut cu_body = Vec::new();
    cu_body.extend_from_slice(b"root2\0");
    let auth_response = scramble_native_password(&scramble, b"secret");
    cu_body.push(auth_response.len() as u8);
    cu_body.extend_from_slice(&auth_response);
    cu_body.push(0); // empty database, NUL-terminated
    cu_body.extend_from_slice(&COLLATION_UTF8MB4.to_le_bytes());
    cu_body.extend_from_slice(AUTH_PLUGIN_NATIVE.as_bytes());
    cu_body.push(0);
    raw_send_command(&mut stream, COM_CHANGE_USER, &cu_body);
    let (_, resp) = read_packet(&mut stream).unwrap();
    assert_eq!(resp[0], OK_HEADER, "{resp:?}");

    let payload = build_execute_payload(prepare_ok.stmt_id, &[], true);
    let resp = raw_execute(&mut stream, &payload);
    assert_eq!(
        resp[0], ERR_HEADER,
        "registry must be cleared by COM_CHANGE_USER"
    );
    let (code, ..) = htap_wire::error_map::parse_err_payload(&resp).unwrap();
    assert_eq!(code, 1243);

    wire.shutdown();
}

/// Finding 3 of the Phase 11 fix pass: a `COM_CHANGE_USER` that triggers an auth-plugin switch
/// must persist the fresh scramble it switched to, not just use it for that one exchange. A real
/// client (confirmed against `mysql-28.0.2`'s `continue_auth`, which writes the `AuthSwitchRequest`
/// scramble straight into `self.0.nonce`) keeps using that same nonce for a *later*
/// `COM_CHANGE_USER` that itself needs no switch — so this server must accept a second
/// `COM_CHANGE_USER`'s auth response computed against the scramble from the first one's switch,
/// not the connection's original handshake scramble.
#[test]
fn test_wire_change_user_reuses_switched_scramble_on_later_change_user() {
    let (_dir, wire) = start(Some("secret"), 4);
    let mut stream = TcpStream::connect(addr(&wire)).unwrap();
    raw_handshake_with_password(&mut stream, b"secret");

    // COM_CHANGE_USER #1: propose a non-native plugin to force an auth-plugin switch. The
    // initial (pre-switch) auth response is irrelevant once a switch is triggered, so it is left
    // empty.
    let mut cu1 = Vec::new();
    cu1.extend_from_slice(b"root\0");
    cu1.push(0); // empty auth response (1-byte length form)
    cu1.push(0); // empty database, NUL-terminated
    cu1.extend_from_slice(&COLLATION_UTF8MB4.to_le_bytes());
    cu1.extend_from_slice(b"mysql_old_password\0");
    raw_send_command(&mut stream, COM_CHANGE_USER, &cu1);

    let (_, switch_payload) = read_packet(&mut stream).unwrap();
    let switch = AuthSwitchRequest::decode(&switch_payload).unwrap();
    assert_eq!(switch.plugin, AUTH_PLUGIN_NATIVE);
    let switched_scramble = switch.scramble;

    let switch_response = scramble_native_password(&switched_scramble, b"secret");
    write_packet(&mut stream, 2, &switch_response).unwrap();
    let (_, resp) = read_packet(&mut stream).unwrap();
    assert_eq!(resp[0], OK_HEADER, "{resp:?}");

    // COM_CHANGE_USER #2: no plugin switch this time, auth response computed against the
    // scramble from the *first* change-user's switch — never the original handshake scramble.
    let mut cu2 = Vec::new();
    cu2.extend_from_slice(b"root\0");
    let auth_response = scramble_native_password(&switched_scramble, b"secret");
    cu2.push(auth_response.len() as u8);
    cu2.extend_from_slice(&auth_response);
    cu2.push(0); // empty database
    cu2.extend_from_slice(&COLLATION_UTF8MB4.to_le_bytes());
    cu2.extend_from_slice(AUTH_PLUGIN_NATIVE.as_bytes());
    cu2.push(0);
    raw_send_command(&mut stream, COM_CHANGE_USER, &cu2);
    let (_, resp) = read_packet(&mut stream).unwrap();
    assert_eq!(
        resp[0], OK_HEADER,
        "COM_CHANGE_USER #2 must succeed against the scramble from #1's auth switch: {resp:?}"
    );

    wire.shutdown();
}

#[test]
fn test_wire_change_user_wrong_password_closes_connection() {
    use std::io::Read;

    let (_dir, wire) = start(Some("secret"), 4);
    let mut stream = TcpStream::connect(addr(&wire)).unwrap();
    let scramble = raw_handshake_with_password(&mut stream, b"secret");

    let mut cu_body = Vec::new();
    cu_body.extend_from_slice(b"root\0");
    let auth_response = scramble_native_password(&scramble, b"wrong");
    cu_body.push(auth_response.len() as u8);
    cu_body.extend_from_slice(&auth_response);
    cu_body.push(0);
    cu_body.extend_from_slice(&COLLATION_UTF8MB4.to_le_bytes());
    cu_body.extend_from_slice(AUTH_PLUGIN_NATIVE.as_bytes());
    cu_body.push(0);
    raw_send_command(&mut stream, COM_CHANGE_USER, &cu_body);
    let (_, resp) = read_packet(&mut stream).unwrap();
    assert_eq!(resp[0], ERR_HEADER);
    let (code, ..) = htap_wire::error_map::parse_err_payload(&resp).unwrap();
    assert_eq!(code, 1045);

    // The connection is closed after a failed COM_CHANGE_USER.
    let mut buf = [0u8; 1];
    let n = stream.read(&mut buf);
    assert!(matches!(n, Ok(0)) || n.is_err(), "got {n:?}");

    wire.shutdown();
}

#[test]
fn test_wire_change_user_while_commit_outcome_pending_stays_quarantined() {
    let (_dir, server, wire) = start_with_server(Some("secret"), 4);
    let mut setup = WireClient::connect(addr(&wire), Some("secret")).unwrap();
    ok(setup
        .query("CREATE TABLE cq (id BIGINT PRIMARY KEY, v INT)")
        .unwrap());

    let mut stream = TcpStream::connect(addr(&wire)).unwrap();
    let scramble = raw_handshake_with_password(&mut stream, b"secret");
    raw_send_command(&mut stream, COM_QUERY, b"BEGIN");
    read_packet(&mut stream).unwrap();
    raw_send_command(
        &mut stream,
        COM_QUERY,
        b"INSERT INTO cq (id, v) VALUES (1, 1)",
    );
    read_packet(&mut stream).unwrap();

    server.txn_manager().set_commit_append_hook(|_journal| {
        Err(HtapError::Io(std::io::Error::other(
            "simulated disk failure",
        )))
    });
    raw_send_command(&mut stream, COM_QUERY, b"COMMIT");
    let (_, resp) = read_packet(&mut stream).unwrap();
    assert_eq!(resp[0], ERR_HEADER);

    let mut cu_body = Vec::new();
    cu_body.extend_from_slice(b"root\0");
    let auth_response = scramble_native_password(&scramble, b"secret");
    cu_body.push(auth_response.len() as u8);
    cu_body.extend_from_slice(&auth_response);
    cu_body.push(0);
    cu_body.extend_from_slice(&COLLATION_UTF8MB4.to_le_bytes());
    cu_body.extend_from_slice(AUTH_PLUGIN_NATIVE.as_bytes());
    cu_body.push(0);
    raw_send_command(&mut stream, COM_CHANGE_USER, &cu_body);
    let (_, resp) = read_packet(&mut stream).unwrap();
    // Correct credentials, but the underlying `Session::reset` is quarantined: the change-user
    // itself reports that stored error rather than pretending to have succeeded, and the
    // connection stays open (this is not an authentication failure).
    assert_eq!(resp[0], ERR_HEADER);
    let (code, ..) = htap_wire::error_map::parse_err_payload(&resp).unwrap();
    assert_eq!(code, 1105);

    raw_send_command(&mut stream, COM_PING, &[]);
    let (_, pong) = read_packet(&mut stream).unwrap();
    assert_eq!(pong[0], OK_HEADER);

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

// ---------------------------------------------------------------------------------------------
// Phase 11 plan task 8: multi-packet messages and max_allowed_packet.
// ---------------------------------------------------------------------------------------------

/// A `COM_QUERY` whose SQL text is >16 MB, exercising message reassembly and `write_message`
/// splitting in both directions: a `SELECT '<literal>'` with no `FROM` and no storage write (so
/// the rowstore commit path's much smaller ~4 MiB payload cap never applies) whose string literal
/// is ~17 MB sends a >16 MB request and receives a >16 MB single-row response back. The same
/// value is round-tripped again through a prepared statement (binary protocol) to exercise
/// `COM_STMT_EXECUTE`'s parameter encoding and the binary resultset row across the same boundary.
#[test]
fn test_wire_large_payload_over_16mb_round_trip() {
    use mysql::prelude::*;

    let (_dir, wire) = start(None, 4);
    // `max_allowed_packet` is deliberately left unset: the `mysql` crate queries
    // `@@max_allowed_packet` during `connect()` and refuses to send anything larger than what it
    // read back (`PlainPacketCodec::encode`'s own `PacketTooLarge` guard) — this server's default
    // of 64 MiB (Phase 11 plan task 8) must be what it reports for the ~17 MB payload below to go
    // through at all, which this test also exercises as a side effect.
    let opts = mysql::OptsBuilder::new()
        .ip_or_hostname(Some("127.0.0.1"))
        .tcp_port(addr(&wire).port())
        .user(Some("root"))
        .prefer_socket(false);
    let mut conn = mysql::Conn::new(opts).unwrap();

    let big = "x".repeat(17 * 1024 * 1024);
    assert!(big.len() > MAX_PACKET_PAYLOAD);

    let sql = format!("SELECT '{big}'");
    let got: String = conn.query_first(&sql).unwrap().unwrap();
    assert_eq!(got, big);

    let stmt = conn.prep("SELECT ?").unwrap();
    let got2: String = conn.exec_first(&stmt, (big.clone(),)).unwrap().unwrap();
    assert_eq!(got2, big);
    conn.close(stmt).unwrap();

    wire.shutdown();
}

/// A server configured with a small `max_allowed_packet` rejects an over-limit message with
/// `ER_NET_PACKET_TOO_LARGE` (1153) and closes the connection. Uses the raw [`WireClient`] rather
/// than the `mysql` crate: a real driver would refuse to *send* the oversize query client-side
/// once it reads back this server's small configured limit (see the previous test's comment), so
/// it would never actually exercise the server's own enforcement.
///
/// The server deliberately never reads (drains) the rejected message's remaining payload bytes
/// (see `read_message_with_stop`'s doc comment: it must not read data it has already decided to
/// reject, to bound memory/time spent on an oversize or malicious message). On Linux, closing a
/// socket with unread bytes still sitting in its receive buffer sends a `RST` rather than a
/// graceful close, which can arrive at the client interleaved with (or instead of) the `ER_NET_
/// PACKET_TOO_LARGE` response it raced to also send — hence the plan's own "if possible" wording
/// for delivering that response. This test accepts either observable outcome: the explicit ERR
/// packet, or a transport-level reset — both are "the connection was rejected and closed".
#[test]
fn test_wire_max_allowed_packet_rejects_oversize_query_and_closes_connection() {
    let (_dir, wire) = start_with_max_allowed_packet(1024 * 1024);
    let mut c = WireClient::connect(addr(&wire), None).unwrap();
    c.ping().unwrap();

    let big_sql = format!("SELECT '{}'", "x".repeat(2 * 1024 * 1024));
    match c.query(&big_sql) {
        Err(WireError::Server { code, .. }) => assert_eq!(code, 1153),
        Err(WireError::Io(_)) => {} // connection reset racing the response; see doc comment above
        other => panic!("expected ER_NET_PACKET_TOO_LARGE or a transport error, got {other:?}"),
    }
    // The connection is closed (or in the process of closing) either way.
    assert!(c.ping().is_err());
    wire.shutdown();
}

/// `@@max_allowed_packet` reflects this server's configured `WireServerConfig::max_allowed_packet`
/// (Phase 11 plan task 8), and `SET max_allowed_packet = ...` stays a read-only no-op (amendment
/// A2 Phase 10 semantics).
#[test]
fn test_max_allowed_packet_variable_reflects_wire_config() {
    let (_dir, wire) = start_with_max_allowed_packet(2 * 1024 * 1024);
    let mut c = WireClient::connect(addr(&wire), None).unwrap();
    assert_eq!(
        rows(c.query("SELECT @@max_allowed_packet").unwrap()),
        vec![Row::new(vec![Value::Int64(2 * 1024 * 1024)])]
    );
    ok(c.query("SET max_allowed_packet = 999").unwrap());
    assert_eq!(
        rows(c.query("SELECT @@max_allowed_packet").unwrap()),
        vec![Row::new(vec![Value::Int64(2 * 1024 * 1024)])]
    );
    wire.shutdown();
}

// ---------------------------------------------------------------------------------------------
// Phase 11 plan task 10: CLIENT_MULTI_STATEMENTS.
// ---------------------------------------------------------------------------------------------

/// Every statement in a `CLIENT_MULTI_STATEMENTS` batch runs in order through
/// `Session::execute_statement`, and every result but the last carries
/// `SERVER_MORE_RESULTS_EXISTS` in its status flags — checked twice: once against the raw packets
/// (so a status-flag regression can't hide behind `WireClient::query_multi`'s own bookkeeping),
/// and once through that client helper for good measure.
#[test]
fn test_wire_multi_statements_sequential_execution_and_more_results_flag() {
    let (_dir, wire) = start(None, 4);

    let mut stream = TcpStream::connect(addr(&wire)).unwrap();
    raw_handshake_multi_statements(&mut stream);
    raw_send_command(
        &mut stream,
        COM_QUERY,
        b"CREATE TABLE ms (id BIGINT PRIMARY KEY, v INT); \
          INSERT INTO ms (id, v) VALUES (1, 10); \
          SELECT id, v FROM ms WHERE id = 1",
    );

    // Result 1: CREATE TABLE -> OK, more results follow.
    let (_, r1) = read_packet(&mut stream).unwrap();
    assert_eq!(r1[0], OK_HEADER, "{r1:?}");
    let ok1 = parse_ok_payload(&r1).unwrap();
    assert_ne!(
        ok1.status & SERVER_MORE_RESULTS_EXISTS,
        0,
        "first of three results must carry SERVER_MORE_RESULTS_EXISTS: {ok1:?}"
    );

    // Result 2: INSERT -> OK, one affected row, more results still follow.
    let (_, r2) = read_packet(&mut stream).unwrap();
    assert_eq!(r2[0], OK_HEADER, "{r2:?}");
    let ok2 = parse_ok_payload(&r2).unwrap();
    assert_eq!(ok2.affected_rows, 1);
    assert_ne!(
        ok2.status & SERVER_MORE_RESULTS_EXISTS,
        0,
        "second of three results must carry SERVER_MORE_RESULTS_EXISTS: {ok2:?}"
    );

    // Result 3: SELECT -> a resultset whose *terminator* does not carry the flag (it is last).
    let (_, count) = read_packet(&mut stream).unwrap();
    assert_eq!(count, vec![2]);
    let (_, _col_id) = read_packet(&mut stream).unwrap();
    let (_, _col_v) = read_packet(&mut stream).unwrap();
    let (_, row) = read_packet(&mut stream).unwrap();
    assert!(!is_resultset_terminator(&row));
    let (_, terminator) = read_packet(&mut stream).unwrap();
    assert!(is_resultset_terminator(&terminator));
    let status = parse_terminator_status(&terminator, true).unwrap();
    assert_eq!(
        status & SERVER_MORE_RESULTS_EXISTS,
        0,
        "the last result must not carry SERVER_MORE_RESULTS_EXISTS: status={status:#x}"
    );

    // Same idea again through `WireClient::query_multi`.
    let mut client = WireClient::connect_with(
        addr(&wire),
        ClientOptions {
            multi_statements: true,
            ..ClientOptions::default()
        },
    )
    .unwrap();
    let outcome = client
        .query_multi("INSERT INTO ms (id, v) VALUES (2, 20); SELECT id, v FROM ms ORDER BY id")
        .unwrap();
    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    let mut results = outcome.results.into_iter();
    assert_eq!(ok(results.next().unwrap()).affected_rows, 1);
    assert_eq!(
        rows(results.next().unwrap()),
        vec![
            Row::new(vec![Value::Int64(1), Value::Int32(10)]),
            Row::new(vec![Value::Int64(2), Value::Int32(20)]),
        ]
    );
    assert!(results.next().is_none());

    wire.shutdown();
}

/// The server stops a batch at its first error, sends that error as the final packet, and never
/// runs the statements that would have followed it.
#[test]
fn test_wire_multi_statements_stops_on_first_error() {
    let (_dir, wire) = start(None, 4);
    let mut client = WireClient::connect_with(
        addr(&wire),
        ClientOptions {
            multi_statements: true,
            ..ClientOptions::default()
        },
    )
    .unwrap();
    ok(client
        .query("CREATE TABLE me (id BIGINT PRIMARY KEY)")
        .unwrap());

    let outcome = client
        .query_multi(
            "INSERT INTO me (id) VALUES (1); \
             SELECT * FROM missing_table; \
             INSERT INTO me (id) VALUES (2)",
        )
        .unwrap();
    assert_eq!(
        outcome.results.len(),
        1,
        "only the statement before the error should have a result: {:?}",
        outcome.results
    );
    assert_eq!(ok(outcome.results[0].clone()).affected_rows, 1);
    let err = outcome
        .error
        .expect("expected the batch to stop with an error");
    assert_eq!(server_code(err), 1146);

    // The third statement never ran: only id=1 is present.
    assert_eq!(
        rows(client.query("SELECT id FROM me ORDER BY id").unwrap()),
        vec![Row::new(vec![Value::Int64(1)])]
    );

    wire.shutdown();
}

/// `DurablePending` (and, by the same code path, `RecoveryRequired`) is an ordinary error from
/// `Session::execute_statement`'s point of view, so it stops a batch exactly like any other
/// error: the statements after it never run.
#[test]
fn test_wire_multi_statements_stops_on_durable_pending() {
    let (_dir, server, wire) = start_with_server(None, 4);
    let mut setup = WireClient::connect(addr(&wire), None).unwrap();
    ok(setup
        .query("CREATE TABLE dp (id BIGINT PRIMARY KEY)")
        .unwrap());

    let mut client = WireClient::connect_with(
        addr(&wire),
        ClientOptions {
            multi_statements: true,
            ..ClientOptions::default()
        },
    )
    .unwrap();

    server.txn_manager().set_commit_append_hook(|_journal| {
        Err(HtapError::Io(std::io::Error::other(
            "simulated disk failure during commit record append",
        )))
    });

    let outcome = client
        .query_multi(
            "INSERT INTO dp (id) VALUES (1); \
             INSERT INTO dp (id) VALUES (2); \
             INSERT INTO dp (id) VALUES (3)",
        )
        .unwrap();
    assert!(
        outcome.results.is_empty(),
        "the very first statement's own implicit commit already failed: {:?}",
        outcome.results
    );
    let err = outcome
        .error
        .expect("DurablePending must stop the batch, not be skipped over");
    assert_eq!(
        server_code(err),
        1105,
        "DurablePending maps to ER_UNKNOWN, never 1213"
    );

    // Neither of the two later statements ever ran. Check from a fresh connection: the batch's
    // own connection is now quarantined (`CommitOutcomePending`) by the failed first statement.
    let mut checker = WireClient::connect(addr(&wire), None).unwrap();
    assert!(rows(checker.query("SELECT id FROM dp").unwrap()).is_empty());

    wire.shutdown();
}

/// Without `CLIENT_MULTI_STATEMENTS` negotiated, a `COM_QUERY` containing more than one statement
/// is rejected exactly as it always was (`htap_sql::parse_one`'s "expected exactly one SQL
/// statement" via `Session::execute`), regardless of the fact that the server now advertises the
/// capability to whoever asks for it.
#[test]
fn test_wire_multi_statements_rejected_without_capability() {
    let (_dir, wire) = start(None, 4);
    let mut client = WireClient::connect(addr(&wire), None).unwrap();
    let err = client.query("SELECT 1; SELECT 2").unwrap_err();
    assert_eq!(server_code(err), 1064);
    // The connection is still usable afterwards.
    client.ping().unwrap();
    wire.shutdown();
}

/// `COM_STMT_PREPARE` still rejects multi-statement text even on a connection that negotiated
/// `CLIENT_MULTI_STATEMENTS`: that capability only ever changes how `COM_QUERY` text is handled
/// (see `htap_wire::server`'s `respond_stmt_prepare`, which always calls `htap_sql::parse_one`).
#[test]
fn test_wire_prepare_rejects_multi_statement_text_even_when_negotiated() {
    let (_dir, wire) = start(None, 4);
    let mut stream = TcpStream::connect(addr(&wire)).unwrap();
    raw_handshake_multi_statements(&mut stream);
    let (code, _sqlstate, _message) =
        raw_prepare_result(&mut stream, "SELECT 1; SELECT 2").unwrap_err();
    assert_eq!(code, 1064);
    wire.shutdown();
}

// ---------------------------------------------------------------------------------------------
// Phase 11 plan task 11: shutdown force-close.
// ---------------------------------------------------------------------------------------------

/// A connection blocked mid-packet (some bytes of the current command already read, so
/// `read_fully_or_stop` keeps blocking past the stop flag; see the `htap_wire::server` module's
/// `# Shutdown` docs) is force-closed by `WireServer::shutdown` rather than left to hang it
/// forever.
#[test]
fn test_shutdown_force_closes_connection_blocked_mid_packet() {
    use std::io::{Read, Write};

    let (_dir, wire) = start(None, 4);
    let mut stream = TcpStream::connect(addr(&wire)).unwrap();
    raw_handshake(&mut stream);

    // A 4-byte packet header announcing a 1000-byte payload, followed by only 10 of those 1000
    // bytes: the server has started reading this packet's payload, so it will block waiting for
    // the other 990 bytes, which this test never sends.
    let declared_len: u32 = 1000;
    let mut header = declared_len.to_le_bytes()[..3].to_vec();
    header.push(0); // sequence id
    stream.write_all(&header).unwrap();
    stream.write_all(&[0xab; 10]).unwrap();
    stream.flush().unwrap();

    let started = std::time::Instant::now();
    wire.shutdown();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "shutdown blocked on the stuck connection for {elapsed:?}"
    );

    // The client observes the server side of the socket going away: EOF, or a reset.
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut buf = [0u8; 16];
    match stream.read(&mut buf) {
        Ok(0) => {}
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
        other => panic!("expected EOF or a connection reset, got {other:?}"),
    }
}

/// Force-closing a connection blocked mid-packet still runs the connection thread's normal
/// cleanup: the `read` call returns an error, `run_commands` propagates it, and `handle_connection`
/// rolls back the session's open transaction before the thread exits (with `Session::drop` as a
/// safety net regardless).
#[test]
fn test_shutdown_force_close_rolls_back_open_transaction() {
    use std::io::Write;

    let dir = TempDir::new().unwrap();
    let listen: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    let config = WireServerConfig {
        listen,
        read_timeout: Duration::from_millis(50),
        ..WireServerConfig::default()
    };
    let wire = WireServer::start(config, Arc::clone(&server)).unwrap();
    let a = addr(&wire);

    let mut setup = WireClient::connect(a, None).unwrap();
    ok(setup
        .query("CREATE TABLE fc (id BIGINT PRIMARY KEY)")
        .unwrap());
    drop(setup);

    let mut stream = TcpStream::connect(a).unwrap();
    raw_handshake(&mut stream);
    raw_send_command(&mut stream, COM_QUERY, b"BEGIN");
    let (_, resp) = read_packet(&mut stream).unwrap();
    assert_eq!(resp[0], OK_HEADER, "{resp:?}");
    raw_send_command(&mut stream, COM_QUERY, b"INSERT INTO fc (id) VALUES (1)");
    let (_, resp) = read_packet(&mut stream).unwrap();
    assert_eq!(resp[0], OK_HEADER, "{resp:?}");

    // Block mid-packet on the next command, then force-close via shutdown.
    let declared_len: u32 = 1000;
    let mut header = declared_len.to_le_bytes()[..3].to_vec();
    header.push(0);
    stream.write_all(&header).unwrap();
    stream.write_all(&[0xab; 10]).unwrap();
    stream.flush().unwrap();
    wire.shutdown();
    drop(stream);

    // Reopen a `LocalServer` over the same directory: the never-committed INSERT is absent.
    drop(server);
    let reopened = LocalServer::open(dir.path()).unwrap();
    let reopened = Arc::new(reopened);
    let reopened_wire = WireServer::start(
        WireServerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            read_timeout: Duration::from_millis(50),
            ..WireServerConfig::default()
        },
        Arc::clone(&reopened),
    )
    .unwrap();
    let mut checker = WireClient::connect(addr(&reopened_wire), None).unwrap();
    assert!(rows(checker.query("SELECT id FROM fc").unwrap()).is_empty());
    reopened_wire.shutdown();
}

// ---------------------------------------------------------------------------------------------
// Fix pass (finding 1): pre-auth reads must be bounded before allocation.
// ---------------------------------------------------------------------------------------------

/// A peer that declares an oversize handshake-response length but never sends that many bytes
/// gets its connection closed rather than the server blocking forever trying to read (or, before
/// this fix, allocating) that declared length. The strict proof that the check happens *before*
/// allocation lives at the codec level
/// (`htap_wire::codec::tests::test_message_reassembly_rejects_over_max_allowed_packet_before_full_read`,
/// which counts bytes actually read off a `CountingReader`); this is the wire-level smoke test
/// that the whole pre-auth path is wired through that same bounded reader end to end.
#[test]
fn test_pre_auth_oversize_handshake_rejected_before_allocation() {
    use std::io::{Read, Write};

    let (_dir, wire) = start(None, 4);
    let mut stream = TcpStream::connect(addr(&wire)).unwrap();
    // Drain the server's initial handshake packet so we are exactly at the point where the
    // server is waiting to read the client's handshake response.
    read_packet(&mut stream).unwrap();

    // Declare a payload far larger than the pre-auth cap (64 KiB, itself already the smaller of
    // that constant and the server's configured `max_allowed_packet`), and send only the 4-byte
    // packet header: no payload bytes at all follow.
    let declared_len: u32 = 10 * 1024 * 1024;
    let mut header = declared_len.to_le_bytes()[..3].to_vec();
    header.push(1); // sequence id following the server's handshake (seq 0)
    stream.write_all(&header).unwrap();
    stream.flush().unwrap();

    // The server must reject this from the header alone and close the connection without ever
    // asking us for the (nonexistent) payload bytes: a read on our end observes EOF (or a reset)
    // well within the test's own timeout, never a hang.
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut buf = [0u8; 1];
    let result = stream.read(&mut buf);
    assert!(
        matches!(result, Ok(0)) || result.is_err(),
        "server must close the connection instead of waiting for the declared payload: {result:?}"
    );

    wire.shutdown();
}

// ---------------------------------------------------------------------------------------------
// Fix pass (finding 8): SERVER_STATUS_AUTOCOMMIT/SERVER_STATUS_IN_TRANS reflect real session
// state instead of being hardcoded (always-on autocommit, never in-trans).
// ---------------------------------------------------------------------------------------------

/// `BEGIN` sets `SERVER_STATUS_IN_TRANS`; `COMMIT` clears it; `SET autocommit = 0` clears
/// `SERVER_STATUS_AUTOCOMMIT` immediately but only sets `SERVER_STATUS_IN_TRANS` once the next
/// statement opens the implicit transaction — checked against raw packets so no client-side
/// bookkeeping can hide a regression.
#[test]
fn test_wire_status_flags_reflect_autocommit_and_transaction_state() {
    const SERVER_STATUS_IN_TRANS: u16 = 0x0001;
    const SERVER_STATUS_AUTOCOMMIT: u16 = 0x0002;

    let (_dir, wire) = start(None, 4);
    let mut stream = TcpStream::connect(addr(&wire)).unwrap();
    raw_handshake(&mut stream);

    raw_send_command(
        &mut stream,
        COM_QUERY,
        b"CREATE TABLE st (id INT PRIMARY KEY)",
    );
    let (_, resp) = read_packet(&mut stream).unwrap();
    let status = parse_ok_payload(&resp).unwrap().status;
    assert_eq!(
        status & (SERVER_STATUS_AUTOCOMMIT | SERVER_STATUS_IN_TRANS),
        SERVER_STATUS_AUTOCOMMIT,
        "fresh connection: autocommit on, no transaction"
    );

    raw_send_command(&mut stream, COM_QUERY, b"BEGIN");
    let (_, resp) = read_packet(&mut stream).unwrap();
    let status = parse_ok_payload(&resp).unwrap().status;
    assert_ne!(
        status & SERVER_STATUS_IN_TRANS,
        0,
        "BEGIN must set SERVER_STATUS_IN_TRANS: {status:#06x}"
    );
    assert_ne!(
        status & SERVER_STATUS_AUTOCOMMIT,
        0,
        "autocommit itself is unaffected by BEGIN: {status:#06x}"
    );

    raw_send_command(&mut stream, COM_QUERY, b"COMMIT");
    let (_, resp) = read_packet(&mut stream).unwrap();
    let status = parse_ok_payload(&resp).unwrap().status;
    assert_eq!(
        status & SERVER_STATUS_IN_TRANS,
        0,
        "COMMIT must clear SERVER_STATUS_IN_TRANS: {status:#06x}"
    );

    raw_send_command(&mut stream, COM_QUERY, b"SET autocommit = 0");
    let (_, resp) = read_packet(&mut stream).unwrap();
    let status = parse_ok_payload(&resp).unwrap().status;
    assert_eq!(
        status & SERVER_STATUS_AUTOCOMMIT,
        0,
        "SET autocommit = 0 must clear SERVER_STATUS_AUTOCOMMIT: {status:#06x}"
    );
    assert_eq!(
        status & SERVER_STATUS_IN_TRANS,
        0,
        "SET autocommit = 0 alone does not yet open an implicit transaction: {status:#06x}"
    );

    // The next statement opens the implicit transaction (autocommit off): its resultset
    // terminator must show SERVER_STATUS_IN_TRANS, with autocommit still off.
    raw_send_command(&mut stream, COM_QUERY, b"SELECT id FROM st");
    let (_, first) = read_packet(&mut stream).unwrap();
    let mut pos = 0;
    let col_count = read_lenenc_int(&first, &mut pos).unwrap();
    assert_eq!(col_count, 1);
    let (_, _col_def) = read_packet(&mut stream).unwrap();
    let (_, terminator) = read_packet(&mut stream).unwrap();
    assert!(is_resultset_terminator(&terminator));
    // `raw_handshake` negotiates CLIENT_DEPRECATE_EOF, so this terminator is OK-shaped.
    let status = parse_ok_payload(&terminator).unwrap().status;
    assert_eq!(
        status & SERVER_STATUS_AUTOCOMMIT,
        0,
        "still off: {status:#06x}"
    );
    assert_ne!(
        status & SERVER_STATUS_IN_TRANS,
        0,
        "the implicit transaction opened by this statement must show SERVER_STATUS_IN_TRANS: \
         {status:#06x}"
    );

    wire.shutdown();
}
