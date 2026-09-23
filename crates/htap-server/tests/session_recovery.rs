//! Cross-cutting durability evidence for Phase 10 sessions (task 10): a session's buffered
//! writes never survive a reopen unless `COMMIT` actually ran the write set through 2PC.
//!
//! `LocalServer::open` holds an exclusive advisory lock on its root for as long as the
//! `LocalServer` (and therefore every `Arc` clone of it, including one held by each open
//! `Session`) is alive, so every test here drops the server and any session before reopening.

use std::sync::Arc;

use htap_common::types::{Row, Value};
use htap_common::HtapError;
use htap_server::LocalServer;
use htap_sql::result::StatementResult;
use tempfile::TempDir;

fn as_rows(result: StatementResult) -> Vec<Row> {
    match result {
        StatementResult::Query(q) => q.rows,
        other => panic!("expected Query result, got {other:?}"),
    }
}

#[test]
fn test_uncommitted_writes_never_visible_after_reopen() {
    let dir = TempDir::new().unwrap();
    {
        let server = Arc::new(LocalServer::open(dir.path()).unwrap());
        server
            .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
            .unwrap();
        server
            .execute("INSERT INTO t (id, v) VALUES (1, 10);")
            .unwrap();

        let mut session = server.open_session().unwrap();
        session.begin().unwrap();
        session
            .execute("INSERT INTO t (id, v) VALUES (2, 20);")
            .unwrap();
        session
            .execute("UPDATE t SET v = 999 WHERE id = 1;")
            .unwrap();
        // Read-your-own-writes confirms the buffered state exists before it is abandoned.
        assert_eq!(
            as_rows(session.execute("SELECT v FROM t WHERE id = 1;").unwrap()),
            vec![Row::new(vec![Value::Int32(999)])]
        );

        // `session` and `server` (the only `Arc<LocalServer>`) both drop here without a
        // `COMMIT`, releasing the root's advisory lock.
    }

    let reopened = LocalServer::open(dir.path()).unwrap();
    assert_eq!(
        as_rows(reopened.execute("SELECT v FROM t WHERE id = 1;").unwrap()),
        vec![Row::new(vec![Value::Int32(10)])],
        "an uncommitted buffered UPDATE must never survive a reopen"
    );
    assert!(
        as_rows(reopened.execute("SELECT id FROM t WHERE id = 2;").unwrap()).is_empty(),
        "an uncommitted buffered INSERT must never survive a reopen"
    );
}

#[test]
fn test_uncommitted_delete_by_filter_vanishes_after_reopen() {
    let dir = TempDir::new().unwrap();
    {
        let server = Arc::new(LocalServer::open(dir.path()).unwrap());
        server
            .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
            .unwrap();
        server
            .execute("INSERT INTO t (id, v) VALUES (1, 10), (2, 20), (3, 30);")
            .unwrap();

        let mut session = server.open_session().unwrap();
        session.begin().unwrap();
        session.execute("DELETE FROM t WHERE v >= 20;").unwrap();
        assert_eq!(
            as_rows(session.execute("SELECT id FROM t ORDER BY id;").unwrap()),
            vec![Row::new(vec![Value::Int64(1)])]
        );

        // `session` and `server` drop without committing the buffered deletes.
    }

    let reopened = LocalServer::open(dir.path()).unwrap();
    assert_eq!(
        as_rows(reopened.execute("SELECT id FROM t ORDER BY id;").unwrap()),
        vec![
            Row::new(vec![Value::Int64(1)]),
            Row::new(vec![Value::Int64(2)]),
            Row::new(vec![Value::Int64(3)]),
        ],
        "an uncommitted filtered DELETE must never survive a reopen"
    );
}

#[test]
fn test_committed_transaction_visible_after_reopen() {
    let dir = TempDir::new().unwrap();
    {
        let server = Arc::new(LocalServer::open(dir.path()).unwrap());
        server
            .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
            .unwrap();

        let mut session = server.open_session().unwrap();
        session.begin().unwrap();
        session
            .execute("INSERT INTO t (id, v) VALUES (1, 10);")
            .unwrap();
        session
            .execute("INSERT INTO t (id, v) VALUES (2, 20);")
            .unwrap();
        session
            .execute("UPDATE t SET v = 999 WHERE id = 1;")
            .unwrap();
        session.commit().unwrap();
        assert!(!session.in_transaction());

        // Dropped here, after a real `COMMIT`.
    }

    let reopened = LocalServer::open(dir.path()).unwrap();
    assert_eq!(
        as_rows(
            reopened
                .execute("SELECT id, v FROM t ORDER BY id;")
                .unwrap()
        ),
        vec![
            Row::new(vec![Value::Int64(1), Value::Int32(999)]),
            Row::new(vec![Value::Int64(2), Value::Int32(20)]),
        ],
        "a committed transaction's buffered writes must survive a reopen"
    );

    // The recovered rowstore continues the same shared version domain: a further autocommit
    // write after reopen just works.
    let ins = reopened
        .execute("INSERT INTO t (id, v) VALUES (3, 30);")
        .unwrap();
    match ins {
        StatementResult::Command(cmd) => assert_eq!(cmd.affected(), 1),
        other => panic!("expected a Command result, got {other:?}"),
    }
}

/// Storage-reviewer-requested test: a session `COMMIT` that returns `DurablePending` because the
/// commit record's fsync was interrupted (not because the record failed to reach the journal
/// file at all) must resolve to a *consistent* outcome after a reopen — the recovered state must
/// agree with what actually landed in the journal, and the write must never be applied more than
/// once.
///
/// `TransactionManager::set_commit_sync_hook` fires after the commit record's bytes are already
/// written via the journal's normal (non-atomic, unsynced) append, immediately before the sync
/// call that would durably flush them; injecting a failure there therefore leaves the record
/// physically present in the journal file (this test never actually kills the process, so there
/// is no real crash to tear the write off), and `recover()` on reopen finds and replays it as
/// committed. That is the "row visible exactly once" outcome the test asserts; see the doc
/// comment on `htap_txn::manager::TransactionManager::commit` for why every failure after the
/// commit record begins appending must be `DurablePending`, never reported as rolled back.
#[test]
fn test_reopen_after_session_commit_durable_pending_resolves_outcome() {
    let dir = TempDir::new().unwrap();
    {
        let server = Arc::new(LocalServer::open(dir.path()).unwrap());
        server
            .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
            .unwrap();
        server
            .execute("INSERT INTO t (id, v) VALUES (1, 10);")
            .unwrap();

        let mut session = server.open_session().unwrap();
        session.begin().unwrap();
        session
            .execute("INSERT INTO t (id, v) VALUES (2, 20);")
            .unwrap();

        server
            .txn_manager()
            .expect("transaction manager is available")
            .set_commit_sync_hook(|_journal| {
                Err(HtapError::Io(std::io::Error::other(
                    "simulated fsync failure after the commit record was already appended",
                )))
            });

        let err = session.commit().unwrap_err();
        assert!(
            err.is_durable_pending(),
            "expected DurablePending, got {err:?}"
        );
        // The session is quarantined until recovery resolves the ambiguity (F1/session module
        // docs): every further statement, including `ROLLBACK`, is rejected with the same
        // stored `DurablePending`, never silently treated as rolled back.
        let rollback_err = session.rollback().unwrap_err();
        assert!(rollback_err.is_durable_pending());

        // `session` and `server` (the only `Arc<LocalServer>`) both drop here, releasing the
        // root's advisory lock, without ever learning whether the commit actually landed.
    }

    // Reopen: `LocalServer::open` runs `TransactionManager::recover()`, which resolves the
    // ambiguity one way or the other from the journal's actual contents.
    let reopened = LocalServer::open(dir.path()).unwrap();
    let rows = as_rows(
        reopened
            .execute("SELECT id, v FROM t ORDER BY id;")
            .unwrap(),
    );

    // The row is visible exactly once (this failure mode's commit record was already durably
    // appended, just not yet fsynced when the process "crashed" — recovery correctly replays
    // it) or entirely absent (never both, and never duplicated) — either is a consistent
    // resolution; only a torn or double application would be a bug.
    let with_id_2 = rows
        .iter()
        .filter(|r| r.get(0) == Some(&Value::Int64(2)))
        .count();
    assert!(
        with_id_2 <= 1,
        "row id=2 must appear at most once after recovery, got {rows:?}"
    );
    if with_id_2 == 1 {
        assert_eq!(
            rows,
            vec![
                Row::new(vec![Value::Int64(1), Value::Int32(10)]),
                Row::new(vec![Value::Int64(2), Value::Int32(20)]),
            ],
            "if the write resolved as committed, its value must be exactly what was buffered"
        );
    } else {
        assert_eq!(
            rows,
            vec![Row::new(vec![Value::Int64(1), Value::Int32(10)])],
            "if the write resolved as not committed, only the pre-existing row remains"
        );
    }

    // The recovered rowstore continues the same shared version domain regardless of which way
    // the ambiguity resolved: a further autocommit write after reopen just works.
    let ins = reopened
        .execute("INSERT INTO t (id, v) VALUES (3, 30);")
        .unwrap();
    match ins {
        StatementResult::Command(cmd) => assert_eq!(cmd.affected(), 1),
        other => panic!("expected a Command result, got {other:?}"),
    }
}

#[test]
fn test_uncommitted_insert_select_vanishes_after_reopen() {
    let dir = TempDir::new().unwrap();
    {
        let server = Arc::new(LocalServer::open(dir.path()).unwrap());
        server
            .execute("CREATE TABLE src (id BIGINT PRIMARY KEY, v INT);")
            .unwrap();
        server
            .execute("CREATE TABLE dst (id BIGINT PRIMARY KEY, v INT);")
            .unwrap();
        server
            .execute("INSERT INTO src (id, v) VALUES (1, 10), (2, 20);")
            .unwrap();

        let mut session = server.open_session().unwrap();
        session.begin().unwrap();
        session
            .execute("INSERT INTO dst (id, v) SELECT id, v FROM src;")
            .unwrap();
        assert_eq!(
            as_rows(
                session
                    .execute("SELECT id, v FROM dst ORDER BY id;")
                    .unwrap()
            ),
            vec![
                Row::new(vec![Value::Int64(1), Value::Int32(10)]),
                Row::new(vec![Value::Int64(2), Value::Int32(20)]),
            ]
        );

        // The buffered INSERT SELECT is abandoned when `session` and `server` drop here.
    }

    let reopened = LocalServer::open(dir.path()).unwrap();
    assert!(as_rows(reopened.execute("SELECT id FROM dst;").unwrap()).is_empty());
    assert_eq!(
        as_rows(
            reopened
                .execute("SELECT id, v FROM src ORDER BY id;")
                .unwrap()
        ),
        vec![
            Row::new(vec![Value::Int64(1), Value::Int32(10)]),
            Row::new(vec![Value::Int64(2), Value::Int32(20)]),
        ]
    );
}

#[test]
fn test_uncommitted_truncate_vanishes_after_reopen() {
    let dir = TempDir::new().unwrap();
    {
        let server = Arc::new(LocalServer::open(dir.path()).unwrap());
        server
            .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
            .unwrap();
        server
            .execute("INSERT INTO t (id, v) VALUES (1, 10), (2, 20), (3, 30);")
            .unwrap();

        let mut session = server.open_session().unwrap();
        session.begin().unwrap();
        session.execute("TRUNCATE TABLE t;").unwrap();
        assert!(as_rows(session.execute("SELECT id FROM t;").unwrap()).is_empty());

        // `session` and `server` drop without committing the buffered truncate.
    }

    let reopened = LocalServer::open(dir.path()).unwrap();
    assert_eq!(
        as_rows(reopened.execute("SELECT id FROM t ORDER BY id;").unwrap()),
        vec![
            Row::new(vec![Value::Int64(1)]),
            Row::new(vec![Value::Int64(2)]),
            Row::new(vec![Value::Int64(3)]),
        ],
        "an uncommitted TRUNCATE must never survive a reopen"
    );
}

#[test]
fn test_non_finite_float_update_reports_reopen_outcome() {
    let dir = TempDir::new().unwrap();
    {
        let server = Arc::new(LocalServer::open(dir.path()).unwrap());
        server
            .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v DOUBLE);")
            .unwrap();
        server
            .execute("INSERT INTO t (id, v) VALUES (1, 1e308);")
            .unwrap();

        let err = server
            .execute("UPDATE t SET v = v * 10 WHERE id = 1;")
            .unwrap_err();
        assert!(
            err.to_string().contains("DOUBLE value is out of range"),
            "expected DOUBLE range error, got {err}"
        );

        // `server` drops here before reopen, releasing the root's advisory lock.
    }

    let reopened = LocalServer::open(dir.path()).unwrap();
    assert_eq!(
        as_rows(reopened.execute("SELECT v FROM t WHERE id = 1;").unwrap()),
        vec![Row::new(vec![Value::Float64(1e308)])],
        "the rejected non-finite UPDATE must not alter the original value"
    );
}

#[test]
fn test_float_multiply_overflow_rejected() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v DOUBLE);")
        .unwrap();
    server
        .execute("INSERT INTO t (id, v) VALUES (1, 1e308);")
        .unwrap();

    let err = server
        .execute("UPDATE t SET v = v * 10 WHERE id = 1;")
        .unwrap_err();
    assert!(
        err.to_string().contains("DOUBLE value is out of range"),
        "expected DOUBLE range error, got {err}"
    );
}

#[test]
fn test_float_divide_by_tiny_overflow_rejected() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v DOUBLE);")
        .unwrap();
    server
        .execute("INSERT INTO t (id, v) VALUES (1, 1e308);")
        .unwrap();

    let err = server
        .execute("UPDATE t SET v = v / 1e-308 WHERE id = 1;")
        .unwrap_err();
    assert!(
        err.to_string().contains("DOUBLE value is out of range"),
        "expected DOUBLE range error, got {err}"
    );
}

#[test]
fn test_float_add_overflow_rejected() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v DOUBLE);")
        .unwrap();
    server
        .execute("INSERT INTO t (id, v) VALUES (1, 1e308);")
        .unwrap();

    let err = server
        .execute("UPDATE t SET v = v + 1e308 WHERE id = 1;")
        .unwrap_err();
    assert!(
        err.to_string().contains("DOUBLE value is out of range"),
        "expected DOUBLE range error, got {err}"
    );
}

#[test]
fn test_float_subtract_overflow_rejected() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v DOUBLE);")
        .unwrap();
    server
        .execute("INSERT INTO t (id, v) VALUES (1, 1e308);")
        .unwrap();

    let err = server
        .execute("UPDATE t SET v = v - (-1e308) WHERE id = 1;")
        .unwrap_err();
    assert!(
        err.to_string().contains("DOUBLE value is out of range"),
        "expected DOUBLE range error, got {err}"
    );
}

#[test]
fn test_double_values_round_trip_bit_identically_after_reopen() {
    let dir = TempDir::new().unwrap();
    let expected = [
        (1_i64, 0.1_f64),
        (2_i64, -0.0_f64),
        (3_i64, 1.000_000_000_000_000_2_f64),
        (4_i64, 1.234_567_890_123_456_7_f64),
        (5_i64, 1e-308_f64),
        (6_i64, f64::MIN_POSITIVE),
        (7_i64, f64::from_bits(1)),
        (8_i64, f64::MAX),
    ];

    {
        let server = LocalServer::open(dir.path()).unwrap();
        server
            .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v DOUBLE);")
            .unwrap();

        let values = expected
            .iter()
            .map(|(id, value)| {
                let value = if *value == 0.0 && value.is_sign_negative() {
                    "-0.0".to_string()
                } else {
                    value.to_string()
                };
                format!("({id}, {value})")
            })
            .collect::<Vec<_>>()
            .join(", ");
        server
            .execute(&format!("INSERT INTO t (id, v) VALUES {values};"))
            .unwrap();
    }

    let reopened = LocalServer::open(dir.path()).unwrap();
    let rows = as_rows(
        reopened
            .execute("SELECT id, v FROM t ORDER BY id;")
            .unwrap(),
    );
    assert_eq!(rows.len(), expected.len());

    for (row, (expected_id, expected_value)) in rows.iter().zip(expected) {
        assert_eq!(row.get(0), Some(&Value::Int64(expected_id)));
        let Value::Float64(actual_value) = row.get(1).unwrap() else {
            panic!("expected DOUBLE value, got {row:?}");
        };
        assert_eq!(
            actual_value.to_bits(),
            expected_value.to_bits(),
            "DOUBLE value for id={expected_id} must preserve its exact bit pattern"
        );
    }
}
