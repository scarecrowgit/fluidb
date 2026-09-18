//! End-to-end TLS tests for `WireServer`.

use std::fs;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use htap_common::types::{Row, Value};
use htap_server::LocalServer;
use htap_wire::{
    ClientOptions, TlsConfig, TlsMode, WireClient, WireError, WireResult, WireServer,
    WireServerConfig,
};
use tempfile::TempDir;

fn cert_files(dir: &TempDir, name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let cert =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .unwrap();
    let cert_path = dir.path().join(format!("{name}-cert.pem"));
    let key_path = dir.path().join(format!("{name}-key.pem"));
    fs::write(&cert_path, cert.cert.pem()).unwrap();
    fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();
    (cert_path, key_path)
}

fn start_tls(require_secure_transport: bool) -> (TempDir, WireServer, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let (cert_path, key_path) = cert_files(&dir, "server");
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    let wire = WireServer::start(
        WireServerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            tls: Some(TlsConfig {
                cert_path: cert_path.clone(),
                key_path,
            }),
            require_secure_transport,
            read_timeout: Duration::from_millis(50),
            ..WireServerConfig::default()
        },
        server,
    )
    .unwrap();
    (dir, wire, cert_path)
}

fn addr(wire: &WireServer) -> SocketAddr {
    wire.local_addr()
}

fn required_tls(ca_cert: std::path::PathBuf) -> TlsMode {
    TlsMode::Required {
        ca_cert,
        server_name: Some("localhost".into()),
    }
}

fn rows(result: WireResult) -> Vec<Row> {
    match result {
        WireResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn test_wire_tls_handshake_round_trip() {
    let (_dir, wire, ca_cert) = start_tls(false);
    let mut client = WireClient::connect_with(
        addr(&wire),
        ClientOptions {
            tls: required_tls(ca_cert),
            ..ClientOptions::default()
        },
    )
    .unwrap();

    client
        .query("CREATE TABLE tls_t (id INT PRIMARY KEY, v VARCHAR(32))")
        .unwrap();
    client
        .query("INSERT INTO tls_t (id, v) VALUES (1, 'secure')")
        .unwrap();
    assert_eq!(
        rows(client.query("SELECT v FROM tls_t WHERE id = 1").unwrap()),
        vec![Row::new(vec![Value::String("secure".into())])]
    );

    let stmt = client.prepare("SELECT v FROM tls_t WHERE id = ?").unwrap();
    assert_eq!(
        rows(client.execute_prepared(&stmt, &[Value::Int64(1)]).unwrap()),
        vec![Row::new(vec![Value::String("secure".into())])]
    );
    client.close_stmt(stmt.stmt_id).unwrap();

    wire.shutdown();
}

#[test]
fn test_wire_require_secure_transport_rejects_plaintext_login() {
    let (_dir, wire, _ca_cert) = start_tls(true);
    let err = WireClient::connect(addr(&wire), None).unwrap_err();
    match err {
        WireError::Server { code, .. } => assert_eq!(code, 1045),
        other => panic!("expected access-denied error, got {other:?}"),
    }
    wire.shutdown();
}

#[test]
fn test_wire_client_tls_required_against_non_tls_server_fails() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    let wire = WireServer::start(
        WireServerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            read_timeout: Duration::from_millis(50),
            ..WireServerConfig::default()
        },
        server,
    )
    .unwrap();
    let (ca_cert, _key_path) = cert_files(&dir, "unused");

    assert!(WireClient::connect_with(
        addr(&wire),
        ClientOptions {
            tls: required_tls(ca_cert),
            ..ClientOptions::default()
        },
    )
    .is_err());

    wire.shutdown();
}

#[test]
fn test_wire_tls_ca_mismatch_rejected() {
    let (dir, wire, _server_cert) = start_tls(false);
    let (wrong_ca, _wrong_key) = cert_files(&dir, "wrong-ca");

    assert!(WireClient::connect_with(
        addr(&wire),
        ClientOptions {
            tls: required_tls(wrong_ca),
            ..ClientOptions::default()
        },
    )
    .is_err());

    wire.shutdown();
}

#[test]
fn test_wire_tls_invalid_cert_path_fails_start() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    let err = WireServer::start(
        WireServerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            tls: Some(TlsConfig {
                cert_path: dir.path().join("missing-cert.pem"),
                key_path: dir.path().join("missing-key.pem"),
            }),
            ..WireServerConfig::default()
        },
        server,
    )
    .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn test_mysql_crate_driver_interop_over_tls() {
    use mysql::prelude::*;

    let (_dir, wire, ca_cert) = start_tls(false);
    let opts = mysql::OptsBuilder::new()
        .ip_or_hostname(Some(addr(&wire).ip().to_string()))
        .tcp_port(addr(&wire).port())
        .user(Some("root"))
        .prefer_socket(false)
        .ssl_opts(mysql::SslOpts::default().with_root_cert_path(Some(ca_cert)));
    let mut conn = mysql::Conn::new(opts).unwrap();

    let one: Option<i32> = conn.query_first("SELECT 1").unwrap();
    assert_eq!(one, Some(1));

    let stmt = conn.prep("SELECT ?").unwrap();
    let value: Option<i32> = conn.exec_first(&stmt, (42,)).unwrap();
    assert_eq!(value, Some(42));

    wire.shutdown();
}

#[test]
fn test_wire_tls_cert_reload_serves_new_cert_to_new_connections() {
    let (dir, wire, cert_a) = start_tls(false);
    let trusted_cert_a = dir.path().join("trusted-cert-a.pem");
    fs::copy(&cert_a, &trusted_cert_a).unwrap();

    let mut existing_client = WireClient::connect_with(
        addr(&wire),
        ClientOptions {
            tls: required_tls(trusted_cert_a.clone()),
            ..ClientOptions::default()
        },
    )
    .unwrap();
    assert_eq!(
        rows(existing_client.query("SELECT 1").unwrap()),
        vec![Row::new(vec![Value::Int64(1)])]
    );

    let (cert_b, _key_b) = cert_files(&dir, "server");
    wire.reload_tls_certs().unwrap();

    let mut new_client = WireClient::connect_with(
        addr(&wire),
        ClientOptions {
            tls: required_tls(cert_b),
            ..ClientOptions::default()
        },
    )
    .unwrap();
    assert_eq!(
        rows(new_client.query("SELECT 1").unwrap()),
        vec![Row::new(vec![Value::Int64(1)])]
    );

    assert!(WireClient::connect_with(
        addr(&wire),
        ClientOptions {
            tls: required_tls(trusted_cert_a),
            ..ClientOptions::default()
        },
    )
    .is_err());

    assert_eq!(
        rows(existing_client.query("SELECT 1").unwrap()),
        vec![Row::new(vec![Value::Int64(1)])]
    );

    wire.shutdown();
}

#[test]
fn test_wire_tls_cert_reload_failure_keeps_old_cert() {
    let (dir, wire, cert_a) = start_tls(false);
    let trusted_cert_a = dir.path().join("trusted-cert-a.pem");
    fs::copy(&cert_a, &trusted_cert_a).unwrap();

    let _existing_client = WireClient::connect_with(
        addr(&wire),
        ClientOptions {
            tls: required_tls(trusted_cert_a.clone()),
            ..ClientOptions::default()
        },
    )
    .unwrap();

    fs::write(dir.path().join("server-key.pem"), b"not a private key").unwrap();
    assert!(wire.reload_tls_certs().is_err());

    let mut new_client = WireClient::connect_with(
        addr(&wire),
        ClientOptions {
            tls: required_tls(trusted_cert_a),
            ..ClientOptions::default()
        },
    )
    .unwrap();
    assert_eq!(
        rows(new_client.query("SELECT 1").unwrap()),
        vec![Row::new(vec![Value::Int64(1)])]
    );

    wire.shutdown();
}

#[test]
fn test_wire_shutdown_force_closes_idle_tls_connection() {
    let (_dir, wire, ca_cert) = start_tls(false);
    let _client = WireClient::connect_with(
        addr(&wire),
        ClientOptions {
            tls: required_tls(ca_cert),
            ..ClientOptions::default()
        },
    )
    .unwrap();

    let started = Instant::now();
    wire.shutdown();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "TLS shutdown blocked for {:?}",
        started.elapsed()
    );
}

#[test]
fn test_wire_tls_cert_reload_mismatched_key_rejected_keeps_old_cert() {
    let (dir, wire, cert_a) = start_tls(false);
    let trusted_cert_a = dir.path().join("trusted-cert-a.pem");
    fs::copy(&cert_a, &trusted_cert_a).unwrap();

    let (cert_b, _key_b) = cert_files(&dir, "server-b");
    fs::rename(&cert_b, dir.path().join("server-cert.pem")).unwrap();

    assert!(wire.reload_tls_certs().is_err());

    let mut client = WireClient::connect_with(
        addr(&wire),
        ClientOptions {
            tls: required_tls(trusted_cert_a),
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
fn test_wire_tls_start_with_mismatched_cert_and_key_fails() {
    let dir = TempDir::new().unwrap();
    let (cert_a, key_a) = cert_files(&dir, "server-a");
    let (cert_b, _key_b) = cert_files(&dir, "server-b");
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());

    let err = WireServer::start(
        WireServerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            tls: Some(TlsConfig {
                cert_path: cert_b,
                key_path: key_a,
            }),
            ..WireServerConfig::default()
        },
        server,
    )
    .unwrap_err();

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert!(!fs::read(cert_a).unwrap().is_empty());
}
