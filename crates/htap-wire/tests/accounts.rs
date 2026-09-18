use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use htap_common::types::{Row, Value};
use htap_server::LocalServer;
use htap_wire::codec::{read_packet, write_packet};
use htap_wire::handshake::{HandshakeResponse41, HandshakeV10};
use htap_wire::proto::*;
use htap_wire::sha1::scramble_native_password;
use htap_wire::{ClientOptions, WireClient, WireError, WireResult, WireServer, WireServerConfig};
use tempfile::TempDir;

fn start() -> (TempDir, WireServer) {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    let wire = WireServer::start(
        WireServerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            password: Some("rootpw".into()),
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

fn root(wire: &WireServer) -> WireClient {
    WireClient::connect(addr(wire), Some("rootpw")).unwrap()
}

fn user(wire: &WireServer, username: &str, password: &str) -> WireClient {
    WireClient::connect_with(
        addr(wire),
        ClientOptions {
            username: username.into(),
            password: Some(password.into()),
            ..ClientOptions::default()
        },
    )
    .unwrap()
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

fn raw_handshake(stream: &mut std::net::TcpStream, username: &str, password: &str) -> [u8; 20] {
    let (_, payload) = read_packet(stream).unwrap();
    let handshake = HandshakeV10::decode(&payload).unwrap();
    let response = HandshakeResponse41 {
        capability_flags: CLIENT_PROTOCOL_41
            | CLIENT_SECURE_CONNECTION
            | CLIENT_PLUGIN_AUTH
            | CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA
            | CLIENT_DEPRECATE_EOF,
        max_packet_size: 1 << 24,
        charset: COLLATION_UTF8MB4 as u8,
        username: username.into(),
        auth_response: scramble_native_password(&handshake.scramble, password.as_bytes()),
        database: None,
        auth_plugin: Some(AUTH_PLUGIN_NATIVE.into()),
        zstd_compression_level: None,
    };
    write_packet(stream, 1, &response.encode()).unwrap();
    let (_, payload) = read_packet(stream).unwrap();
    assert_eq!(payload[0], OK_HEADER, "{payload:?}");
    handshake.scramble
}

fn raw_query(stream: &mut std::net::TcpStream, sql: &str) {
    let mut payload = vec![COM_QUERY];
    payload.extend_from_slice(sql.as_bytes());
    write_packet(stream, 0, &payload).unwrap();
}

fn raw_change_user(
    stream: &mut std::net::TcpStream,
    username: &str,
    password: &str,
    scramble: &[u8; 20],
) {
    let response = scramble_native_password(scramble, password.as_bytes());
    let mut body = Vec::new();
    body.extend_from_slice(username.as_bytes());
    body.push(0);
    body.push(response.len() as u8);
    body.extend_from_slice(&response);
    body.push(0);
    body.extend_from_slice(&COLLATION_UTF8MB4.to_le_bytes());
    body.extend_from_slice(AUTH_PLUGIN_NATIVE.as_bytes());
    body.push(0);

    let mut payload = vec![COM_CHANGE_USER];
    payload.extend_from_slice(&body);
    write_packet(stream, 0, &payload).unwrap();
}

#[test]
fn test_wire_root_login_with_bootstrap_password() {
    let (_dir, wire) = start();
    root(&wire).ping().unwrap();
    wire.shutdown();
}

#[test]
fn test_wire_login_uses_catalog_account_not_shared_password() {
    let (_dir, wire) = start();
    let mut admin = root(&wire);
    admin.query("CREATE USER u IDENTIFIED BY 'upw'").unwrap();

    user(&wire, "u", "upw").ping().unwrap();
    let err = WireClient::connect_with(
        addr(&wire),
        ClientOptions {
            username: "u".into(),
            password: Some("rootpw".into()),
            ..ClientOptions::default()
        },
    )
    .unwrap_err();
    assert_eq!(server_code(err), 1045);

    admin.query("ALTER USER root IDENTIFIED BY 'new'").unwrap();
    assert_eq!(
        server_code(WireClient::connect(addr(&wire), Some("rootpw")).unwrap_err()),
        1045
    );
    WireClient::connect(addr(&wire), Some("new")).unwrap();
    wire.shutdown();
}

#[test]
fn test_wire_unknown_user_and_wrong_password_same_error() {
    let (_dir, wire) = start();
    let unknown = WireClient::connect_with(
        addr(&wire),
        ClientOptions {
            username: "missing".into(),
            password: Some("pw".into()),
            ..ClientOptions::default()
        },
    )
    .unwrap_err();
    let wrong = WireClient::connect(addr(&wire), Some("wrong")).unwrap_err();

    match (unknown, wrong) {
        (
            WireError::Server {
                code: unknown_code,
                message: unknown_message,
                ..
            },
            WireError::Server {
                code: wrong_code,
                message: wrong_message,
                ..
            },
        ) => {
            assert_eq!(unknown_code, 1045);
            assert_eq!(wrong_code, 1045);
            assert_eq!(
                unknown_message.replace("'missing'", "'root'"),
                wrong_message
            );
        }
        other => panic!("expected access-denied errors, got {other:?}"),
    }
    wire.shutdown();
}

#[test]
fn test_wire_privileges_enforced_over_text_protocol() {
    let (_dir, wire) = start();
    let mut admin = root(&wire);
    admin
        .query("CREATE TABLE a (id BIGINT PRIMARY KEY, v INT)")
        .unwrap();
    admin.query("INSERT INTO a (id, v) VALUES (1, 10)").unwrap();
    admin.query("CREATE USER u IDENTIFIED BY 'upw'").unwrap();

    let mut account = user(&wire, "u", "upw");
    assert_eq!(
        server_code(account.query("SELECT * FROM a").unwrap_err()),
        1146
    );

    admin.query("GRANT SELECT ON a TO u").unwrap();
    assert_eq!(
        rows(account.query("SELECT v FROM a WHERE id = 1").unwrap()),
        vec![Row::new(vec![Value::Int32(10)])]
    );
    assert_eq!(
        server_code(
            account
                .query("INSERT INTO a (id, v) VALUES (2, 20)")
                .unwrap_err()
        ),
        1142
    );
    wire.shutdown();
}

#[test]
fn test_wire_prepare_masks_invisible_table() {
    let (_dir, wire) = start();
    let mut admin = root(&wire);
    admin
        .query("CREATE TABLE a (id BIGINT PRIMARY KEY)")
        .unwrap();
    admin.query("CREATE USER u IDENTIFIED BY 'upw'").unwrap();
    let mut account = user(&wire, "u", "upw");

    let hidden = account.prepare("SELECT * FROM a").unwrap_err();
    let missing = account.prepare("SELECT * FROM missing").unwrap_err();
    match (hidden, missing) {
        (
            WireError::Server {
                code: hidden_code,
                message: hidden_message,
                ..
            },
            WireError::Server {
                code: missing_code,
                message: missing_message,
                ..
            },
        ) => {
            assert_eq!(hidden_code, 1146);
            assert_eq!(missing_code, 1146);
            assert_eq!(hidden_message.replace("'a'", "'missing'"), missing_message);
        }
        other => panic!("expected masked prepare errors, got {other:?}"),
    }
    wire.shutdown();
}

#[test]
fn test_wire_prepare_with_placeholders_works_for_granted_table() {
    let (_dir, wire) = start();
    let mut admin = root(&wire);
    admin
        .query("CREATE TABLE a (id BIGINT PRIMARY KEY, v INT)")
        .unwrap();
    admin.query("INSERT INTO a (id, v) VALUES (1, 10)").unwrap();
    admin.query("CREATE USER u IDENTIFIED BY 'upw'").unwrap();
    admin.query("GRANT SELECT ON a TO u").unwrap();

    let mut account = user(&wire, "u", "upw");
    let stmt = account.prepare("SELECT v FROM a WHERE id = ?").unwrap();
    assert_eq!(
        rows(account.execute_prepared(&stmt, &[Value::Int64(1)]).unwrap()),
        vec![Row::new(vec![Value::Int32(10)])]
    );
    wire.shutdown();
}

#[test]
fn test_wire_execute_after_revoke_denied() {
    let (_dir, wire) = start();
    let mut admin = root(&wire);
    admin
        .query("CREATE TABLE a (id BIGINT PRIMARY KEY)")
        .unwrap();
    admin.query("CREATE USER u IDENTIFIED BY 'upw'").unwrap();
    admin.query("GRANT SELECT ON a TO u").unwrap();

    let mut account = user(&wire, "u", "upw");
    let stmt = account.prepare("SELECT * FROM a WHERE id = ?").unwrap();
    admin.query("REVOKE SELECT ON a FROM u").unwrap();
    assert_eq!(
        server_code(
            account
                .execute_prepared(&stmt, &[Value::Int64(1)])
                .unwrap_err()
        ),
        1146
    );
    wire.shutdown();
}

#[test]
fn test_wire_privileges_enforced_in_multi_statement_batch() {
    let (_dir, wire) = start();
    let mut admin = root(&wire);
    admin
        .query("CREATE TABLE a (id BIGINT PRIMARY KEY)")
        .unwrap();
    admin.query("CREATE USER u IDENTIFIED BY 'upw'").unwrap();
    admin.query("GRANT SELECT ON a TO u").unwrap();

    let mut account = WireClient::connect_with(
        addr(&wire),
        ClientOptions {
            username: "u".into(),
            password: Some("upw".into()),
            multi_statements: true,
            ..ClientOptions::default()
        },
    )
    .unwrap();
    let outcome = account
        .query_multi("SELECT * FROM a; INSERT INTO a (id) VALUES (1)")
        .unwrap();
    assert_eq!(outcome.results.len(), 1);
    assert_eq!(server_code(outcome.error.unwrap()), 1142);
    wire.shutdown();
}

#[test]
fn test_wire_change_user_to_different_account_switches_privileges() {
    let (_dir, wire) = start();
    let mut admin = root(&wire);
    admin
        .query("CREATE TABLE a (id BIGINT PRIMARY KEY)")
        .unwrap();
    admin.query("CREATE USER u IDENTIFIED BY 'upw'").unwrap();
    admin.query("CREATE USER v IDENTIFIED BY 'vpw'").unwrap();
    admin.query("GRANT SELECT ON a TO u").unwrap();

    let mut stream = std::net::TcpStream::connect(addr(&wire)).unwrap();
    let scramble = raw_handshake(&mut stream, "u", "upw");

    raw_query(&mut stream, "SELECT * FROM a");
    let (_, count) = read_packet(&mut stream).unwrap();
    assert_eq!(count, vec![1]);
    read_packet(&mut stream).unwrap();
    read_packet(&mut stream).unwrap();

    raw_change_user(&mut stream, "v", "vpw", &scramble);
    let (_, response) = read_packet(&mut stream).unwrap();
    assert_eq!(response[0], OK_HEADER, "{response:?}");

    raw_query(&mut stream, "SELECT * FROM a");
    let (_, response) = read_packet(&mut stream).unwrap();
    assert_eq!(response[0], ERR_HEADER);
    let (code, ..) = htap_wire::error_map::parse_err_payload(&response).unwrap();
    assert_eq!(code, 1146);
    wire.shutdown();
}

#[test]
fn test_mysql_crate_login_as_catalog_account() {
    let (_dir, wire) = start();
    let mut admin = root(&wire);
    admin.query("CREATE USER u IDENTIFIED BY 'upw'").unwrap();

    let options = mysql::OptsBuilder::new()
        .ip_or_hostname(Some("127.0.0.1"))
        .tcp_port(addr(&wire).port())
        .user(Some("u"))
        .pass(Some("upw"))
        .prefer_socket(false)
        .max_allowed_packet(Some(16 * 1024 * 1024));
    let mut connection = mysql::Conn::new(options).unwrap();
    connection.ping().unwrap();
    wire.shutdown();
}
