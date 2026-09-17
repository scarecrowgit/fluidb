//! `RemoteClient` prepared-statement API (Phase 11 plan task 5): binding through
//! `COM_STMT_PREPARE`/`EXECUTE`/`CLOSE` must produce results identical to `EmbeddedClient`
//! executing the equivalent literal SQL, across every engine `Value` type including `NULL`, and a
//! prepared statement must be safely reusable across several `execute_prepared` calls.

use std::sync::Arc;
use std::time::Duration;

use htap_client::{EmbeddedClient, RemoteClient};
use htap_common::types::Value;
use htap_common::HtapError;
use htap_server::LocalServer;
use htap_wire::{WireServer, WireServerConfig};
use tempfile::TempDir;

const CREATE_TABLE: &str =
    "CREATE TABLE t (id BIGINT PRIMARY KEY, name VARCHAR(32), score DOUBLE, ok BOOL, \
     raw VARBINARY(8), ts TIMESTAMP)";

/// Starts a fresh `htap-wire` server and connects a `RemoteClient` to it. Returns the `TempDir`
/// and `WireServer` too so the caller can keep them alive for the test's duration and shut the
/// server down explicitly at the end.
fn start_remote() -> (TempDir, WireServer, RemoteClient) {
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
    let remote = RemoteClient::connect(wire.local_addr(), None).unwrap();
    (dir, wire, remote)
}

/// Renders an engine [`Value`] as the literal SQL text `EmbeddedClient` would bind identically to
/// what a prepared statement bound as the same value (used to build the "reference" literal SQL
/// executed on the embedded side).
fn literal_of(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
        Value::Int32(i) => i.to_string(),
        Value::Int64(i) => i.to_string(),
        Value::Float64(f) => format!("{f}"),
        Value::String(s) => format!("'{}'", s.replace('\'', "''")),
        Value::Bytes(b) => format!(
            "X'{}'",
            b.iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ),
        Value::Timestamp(micros) => micros.to_string(),
    }
}

/// Every column value type this test round-trips: `Int64` (the primary key, never NULL),
/// `String`, `Float64`, `Bool`, `Bytes`, and `Timestamp` — plus an all-NULL row (apart from the
/// primary key) to cover `NULL` binding for every nullable column in one pass.
fn rows() -> Vec<Vec<Value>> {
    vec![
        vec![
            Value::Int64(1),
            Value::String("ann".into()),
            Value::Float64(1.5),
            Value::Bool(true),
            Value::Bytes(vec![0x0a, 0x0b]),
            Value::Timestamp(1_700_000_000_000_000),
        ],
        vec![
            Value::Int64(2),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ],
        vec![
            Value::Int64(3),
            Value::String("cat".into()),
            Value::Float64(-2.25),
            Value::Bool(false),
            Value::Bytes(vec![]),
            Value::Timestamp(0),
        ],
    ]
}

#[test]
fn test_remote_prepared_statement_matches_embedded_literal_execution() {
    let embedded_dir = TempDir::new().unwrap();
    let embedded = EmbeddedClient::open(embedded_dir.path()).unwrap();
    let (_remote_dir, wire, mut remote) = start_remote();

    // CREATE TABLE: DDL, no version to keep in lockstep, executed identically on both sides.
    let expected = embedded.execute(CREATE_TABLE).unwrap();
    let actual = remote.execute(CREATE_TABLE).unwrap();
    assert_eq!(expected, actual);

    // INSERT, once per row, through a single prepared statement reused across every execute:
    // every commit happens 1:1 on both sides, in the same order, so the embedded and remote
    // servers' internal commit versions stay in lockstep and full `StatementResult` equality
    // (including the `Dml` version) holds, not just row content.
    let insert_sql = "INSERT INTO t (id, name, score, ok, raw, ts) VALUES (?, ?, ?, ?, ?, ?)";
    let insert_stmt = remote.prepare(insert_sql).unwrap();
    assert_eq!(insert_stmt.num_params, 6);
    for row in rows() {
        let literal = format!(
            "INSERT INTO t (id, name, score, ok, raw, ts) VALUES ({}, {}, {}, {}, {}, {})",
            literal_of(&row[0]),
            literal_of(&row[1]),
            literal_of(&row[2]),
            literal_of(&row[3]),
            literal_of(&row[4]),
            literal_of(&row[5]),
        );
        let expected = embedded.execute(&literal).unwrap();
        let actual = remote.execute_prepared(&insert_stmt, &row).unwrap();
        assert_eq!(expected, actual, "INSERT mismatch for row {row:?}");
    }
    remote.close_prepared(insert_stmt).unwrap();

    // SELECT with a placeholder, reused across several executes with different parameters
    // (including one that matches no row): pure reads, so no version bookkeeping is involved.
    let select_sql = "SELECT id, name, score, ok, raw, ts FROM t WHERE id = ?";
    let select_stmt = remote.prepare(select_sql).unwrap();
    assert_eq!(select_stmt.num_params, 1);
    for id in [1i64, 2, 3, 42] {
        let expected = embedded
            .execute(&format!(
                "SELECT id, name, score, ok, raw, ts FROM t WHERE id = {id}"
            ))
            .unwrap();
        let actual = remote
            .execute_prepared(&select_stmt, &[Value::Int64(id)])
            .unwrap();
        assert_eq!(expected, actual, "SELECT mismatch for id={id}");
    }
    remote.close_prepared(select_stmt).unwrap();

    // UPDATE with placeholders, again kept in lockstep for a full `StatementResult` comparison.
    let update_stmt = remote
        .prepare("UPDATE t SET score = ? WHERE id = ?")
        .unwrap();
    assert_eq!(update_stmt.num_params, 2);
    let expected = embedded
        .execute("UPDATE t SET score = 9.5 WHERE id = 1")
        .unwrap();
    let actual = remote
        .execute_prepared(&update_stmt, &[Value::Float64(9.5), Value::Int64(1)])
        .unwrap();
    assert_eq!(expected, actual, "UPDATE mismatch");
    remote.close_prepared(update_stmt).unwrap();

    let after_update = "SELECT score FROM t WHERE id = 1";
    assert_eq!(
        embedded.execute(after_update).unwrap(),
        remote.execute(after_update).unwrap()
    );

    // DELETE with a placeholder.
    let delete_stmt = remote.prepare("DELETE FROM t WHERE id = ?").unwrap();
    assert_eq!(delete_stmt.num_params, 1);
    let expected = embedded.execute("DELETE FROM t WHERE id = 3").unwrap();
    let actual = remote
        .execute_prepared(&delete_stmt, &[Value::Int64(3)])
        .unwrap();
    assert_eq!(expected, actual, "DELETE mismatch");
    remote.close_prepared(delete_stmt).unwrap();

    let after_delete = "SELECT id FROM t ORDER BY id";
    assert_eq!(
        embedded.execute(after_delete).unwrap(),
        remote.execute(after_delete).unwrap()
    );

    remote.close().unwrap();
    wire.shutdown();
}

#[test]
fn test_remote_prepared_statement_close_then_execute_errors() {
    let (_dir, wire, mut remote) = start_remote();
    remote.execute(CREATE_TABLE).unwrap();

    let stmt = remote.prepare("SELECT id FROM t WHERE id = ?").unwrap();
    remote.close_prepared(stmt.clone()).unwrap();

    let err = remote
        .execute_prepared(&stmt, &[Value::Int64(1)])
        .unwrap_err();
    match err {
        HtapError::Internal(msg) => assert!(
            msg.contains("1243") || msg.to_ascii_lowercase().contains("unknown"),
            "{msg}"
        ),
        other => panic!("expected HtapError::Internal (ER_UNKNOWN_STMT_HANDLER), got {other:?}"),
    }

    remote.close().unwrap();
    wire.shutdown();
}
