//! `RemoteClient` returns the same results as `EmbeddedClient` for the same statements.

use std::sync::Arc;
use std::time::Duration;

use htap_client::{ClientOptions, EmbeddedClient, RemoteClient, StatementResult};
use htap_common::HtapError;
use htap_server::LocalServer;
use htap_wire::{WireServer, WireServerConfig};
use tempfile::TempDir;

const STATEMENTS: &[&str] = &[
    "CREATE TABLE t (id BIGINT PRIMARY KEY, name VARCHAR(32), score DOUBLE, ok BOOL, \
     raw VARBINARY(8), ts TIMESTAMP)",
    "INSERT INTO t (id, name, score, ok, raw, ts) VALUES (1, 'ann', 1.5, TRUE, X'0a0b', 1700000000000000)",
    "INSERT INTO t (id, name, score, ok, raw, ts) VALUES (2, NULL, NULL, NULL, NULL, NULL), \
     (3, 'cat', -2.25, FALSE, X'', 0)",
    "SELECT * FROM t WHERE id = 1",
    "SELECT name, score FROM t WHERE id = 2",
    "SELECT id FROM t WHERE id = 42",
    "SELECT COUNT(*), MAX(score) FROM t",
    "SELECT id, name FROM t WHERE score < 2 ORDER BY id DESC",
    "DELETE FROM t WHERE id = 3",
    "SELECT COUNT(id) FROM t",
];

#[test]
fn test_remote_client_matches_embedded_client_ddl_dml_select() {
    let embedded_dir = TempDir::new().unwrap();
    let embedded = EmbeddedClient::open(embedded_dir.path()).unwrap();

    let remote_dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(remote_dir.path()).unwrap());
    let wire = WireServer::start(
        WireServerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            password: Some("pw".into()),
            read_timeout: Duration::from_millis(50),
            ..WireServerConfig::default()
        },
        server,
    )
    .unwrap();
    let mut remote = RemoteClient::connect(wire.local_addr(), Some("pw")).unwrap();

    for sql in STATEMENTS {
        let a = embedded.execute(sql).unwrap();
        let b = remote.execute(sql).unwrap();
        assert_eq!(a, b, "results differ for {sql}");
    }

    // Error categories match too.
    let cases = [
        "SELEC 1 FROM t",
        "SELECT id FROM missing WHERE id = 1",
        "CREATE TABLE t (id BIGINT PRIMARY KEY)",
        "SELECT name, COUNT(*) OVER () FROM t",
    ];
    for sql in cases {
        let a = embedded.execute(sql).unwrap_err();
        let b = remote.execute(sql).unwrap_err();
        assert_eq!(
            std::mem::discriminant(&a),
            std::mem::discriminant(&b),
            "error category differs for {sql}: {a} vs {b}"
        );
    }
    match remote
        .execute("SELECT id FROM missing WHERE id = 1")
        .unwrap_err()
    {
        HtapError::NotFound(msg) => assert!(msg.contains("missing")),
        other => panic!("{other}"),
    }

    remote.ping().unwrap();
    assert!(remote.server_version().contains("fluidb"));
    remote.close().unwrap();

    // Wrong password is a clear error, not a hang.
    match RemoteClient::connect(wire.local_addr(), Some("bad")) {
        Err(HtapError::Internal(msg)) => assert!(msg.contains("1045"), "{msg}"),
        other => panic!("{other:?}"),
    }
    let mut legacy = RemoteClient::connect_with(
        wire.local_addr(),
        ClientOptions {
            password: Some("pw".into()),
            deprecate_eof: false,
            ..ClientOptions::default()
        },
    )
    .unwrap();
    match legacy.execute("SELECT COUNT(id) FROM t").unwrap() {
        StatementResult::Query(q) => assert_eq!(q.num_rows(), 1),
        other => panic!("{other:?}"),
    }
    wire.shutdown();
}
