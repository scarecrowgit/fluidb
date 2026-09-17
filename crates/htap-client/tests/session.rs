//! `EmbeddedClient::open_session`: explicit transactions on top of the embedded façade,
//! independent of `EmbeddedClient::execute`'s autocommit behavior (Phase 10 task 8).

use htap_client::EmbeddedClient;
use htap_common::types::{Row, Value};
use htap_sql::result::StatementResult;
use tempfile::TempDir;

fn as_rows(result: StatementResult) -> Vec<Row> {
    match result {
        StatementResult::Query(q) => q.rows,
        other => panic!("expected Query result, got {other:?}"),
    }
}

#[test]
fn test_embedded_session_begin_commit_rollback() {
    let dir = TempDir::new().unwrap();
    let client = EmbeddedClient::open(dir.path()).unwrap();
    client
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();

    let mut session = client.open_session();
    session.begin().unwrap();
    session
        .execute("INSERT INTO t (id, v) VALUES (1, 10);")
        .unwrap();

    // Read-your-own-writes inside the session, before COMMIT.
    assert_eq!(
        as_rows(session.execute("SELECT v FROM t WHERE id = 1;").unwrap()),
        vec![Row::new(vec![Value::Int32(10)])]
    );
    // `EmbeddedClient::execute` still only sees committed state.
    assert!(as_rows(client.execute("SELECT id FROM t WHERE id = 1;").unwrap()).is_empty());

    session.commit().unwrap();
    assert_eq!(
        as_rows(client.execute("SELECT v FROM t WHERE id = 1;").unwrap()),
        vec![Row::new(vec![Value::Int32(10)])]
    );

    // A second transaction, rolled back, never becomes visible.
    session.begin().unwrap();
    session
        .execute("UPDATE t SET v = 999 WHERE id = 1;")
        .unwrap();
    session.rollback().unwrap();
    assert!(!session.in_transaction());
    assert_eq!(
        as_rows(client.execute("SELECT v FROM t WHERE id = 1;").unwrap()),
        vec![Row::new(vec![Value::Int32(10)])]
    );
}

#[test]
fn test_embedded_autocommit_execute_unaffected_by_open_session_on_same_server() {
    let dir = TempDir::new().unwrap();
    let client = EmbeddedClient::open(dir.path()).unwrap();
    client
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();
    client
        .execute("INSERT INTO t (id, v) VALUES (1, 1);")
        .unwrap();

    let mut session = client.open_session();
    session.begin().unwrap();
    session.execute("UPDATE t SET v = 2 WHERE id = 1;").unwrap();

    // `EmbeddedClient::execute` keeps auto-committing exactly as before Phase 10, oblivious to
    // the other open session's buffered write.
    let ins = client
        .execute("INSERT INTO t (id, v) VALUES (2, 20);")
        .unwrap();
    match ins {
        StatementResult::Command(cmd) => assert_eq!(cmd.affected(), 1),
        other => panic!("expected Command result, got {other:?}"),
    }
    assert_eq!(
        as_rows(client.execute("SELECT v FROM t WHERE id = 1;").unwrap()),
        vec![Row::new(vec![Value::Int32(1)])],
        "the session's uncommitted write must not leak into autocommit reads"
    );
    assert_eq!(
        as_rows(client.execute("SELECT v FROM t WHERE id = 2;").unwrap()),
        vec![Row::new(vec![Value::Int32(20)])]
    );

    session.commit().unwrap();
    assert_eq!(
        as_rows(client.execute("SELECT v FROM t WHERE id = 1;").unwrap()),
        vec![Row::new(vec![Value::Int32(2)])]
    );
}
