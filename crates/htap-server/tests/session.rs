//! Server-side sessions: session identity, drop-time rollback, write-set payload capping,
//! read-your-own-writes across the point read, narrow analytic scan, and general query paths
//! (Phase 10 tasks 4-5), commit/rollback semantics, poisoning, commit-time catalog
//! revalidation, `DurablePending` handling, and control statements / autocommit (Phase 10
//! tasks 6-7).

use std::sync::Arc;

use htap_catalog::local::LocalCatalogStore;
use htap_catalog::store::CatalogStore;
use htap_catalog::{ConversionDescriptor, ConversionPhase, StorageDescriptor, StorageFormat};
use htap_common::types::{Row, Value};
use htap_common::version::Version;
use htap_common::HtapError;
use htap_server::{LocalServer, Session};
use htap_sql::result::StatementResult;
use tempfile::TempDir;

fn as_rows(result: StatementResult) -> Vec<Row> {
    match result {
        StatementResult::Query(q) => q.rows,
        other => panic!("expected Query result, got {other:?}"),
    }
}

/// Executes `sql` autocommit against `server` and returns the resulting rows.
fn exec_rows(server: &LocalServer, sql: &str) -> Vec<Row> {
    as_rows(server.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}")))
}

/// Executes `sql` inside `session` and returns the resulting rows.
fn session_rows(session: &mut Session, sql: &str) -> Vec<Row> {
    as_rows(
        session
            .execute(sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}")),
    )
}

#[test]
fn test_session_id_unique_and_monotonic() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());

    let s1 = server.open_session();
    let s2 = server.open_session();
    let s3 = server.open_session();

    assert!(s1.id().get() < s2.id().get());
    assert!(s2.id().get() < s3.id().get());
    assert_ne!(s1.id(), s2.id());
}

#[test]
fn test_drop_rolls_back_open_transaction() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("INSERT INTO t (id, v) VALUES (1, 10);")
        .unwrap();

    {
        let mut session = server.open_session();
        session.begin().unwrap();
        session
            .execute("UPDATE t SET v = 999 WHERE id = 1;")
            .unwrap();
        assert!(session.in_transaction());
        assert_eq!(
            session_rows(&mut session, "SELECT v FROM t WHERE id = 1;"),
            vec![Row::new(vec![Value::Int32(999)])]
        );
        // `session` is dropped here without a `commit()`.
    }

    // Nothing was ever written to storage: a fresh autocommit read (and a fresh session) still
    // see only the originally committed value.
    assert_eq!(
        exec_rows(&server, "SELECT v FROM t WHERE id = 1;"),
        vec![Row::new(vec![Value::Int32(10)])]
    );
    let mut fresh = server.open_session();
    assert_eq!(
        session_rows(&mut fresh, "SELECT v FROM t WHERE id = 1;"),
        vec![Row::new(vec![Value::Int32(10)])]
    );
}

#[test]
fn test_write_set_payload_cap_enforced_incrementally_per_statement() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute("CREATE TABLE big (id BIGINT PRIMARY KEY, data VARCHAR);")
        .unwrap();

    let mut session = server.open_session();
    session.begin().unwrap();

    // Each row is ~2 MiB; the write set's 16 MiB cap is hit well before 20 of them, so this
    // loop terminates via the `break` on error, not the safety bound.
    let chunk = "x".repeat(2_000_000);
    let mut inserted = 0usize;
    loop {
        assert!(
            inserted < 20,
            "cap was never enforced; test assumption is wrong"
        );
        let sql = format!("INSERT INTO big (id, data) VALUES ({inserted}, '{chunk}');");
        match session.execute(&sql) {
            Ok(_) => inserted += 1,
            Err(err) => {
                assert!(
                    matches!(err, HtapError::InvalidArgument(_)),
                    "expected InvalidArgument, got {err:?}"
                );
                break;
            }
        }
    }
    assert!(
        inserted > 0,
        "expected at least one row to fit under the cap before the next one is rejected"
    );

    // The rejected statement did not partially or fully pollute the write set: every
    // previously buffered row is still there, and the write set keeps working normally.
    for i in 0..inserted {
        let rows = session_rows(&mut session, &format!("SELECT id FROM big WHERE id = {i};"));
        assert_eq!(rows.len(), 1, "row {i} should still be buffered");
    }
    session
        .execute("INSERT INTO big (id, data) VALUES (999999, 'small');")
        .unwrap();
    assert_eq!(
        session_rows(&mut session, "SELECT id FROM big WHERE id = 999999;").len(),
        1
    );
}

#[test]
fn test_read_your_own_writes_point_select() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("INSERT INTO t (id, v) VALUES (1, 10);")
        .unwrap();

    let mut session = server.open_session();
    session.begin().unwrap();
    session
        .execute("UPDATE t SET v = 20 WHERE id = 1;")
        .unwrap();
    session
        .execute("INSERT INTO t (id, v) VALUES (2, 200);")
        .unwrap();

    // Buffered update and buffered insert are both visible through the point-read path.
    assert_eq!(
        session_rows(&mut session, "SELECT v FROM t WHERE id = 1;"),
        vec![Row::new(vec![Value::Int32(20)])]
    );
    assert_eq!(
        session_rows(&mut session, "SELECT v FROM t WHERE id = 2;"),
        vec![Row::new(vec![Value::Int32(200)])]
    );

    // A separate autocommit read still observes only the committed state.
    assert_eq!(
        exec_rows(&server, "SELECT v FROM t WHERE id = 1;"),
        vec![Row::new(vec![Value::Int32(10)])]
    );
    assert!(exec_rows(&server, "SELECT id FROM t WHERE id = 2;").is_empty());
}

#[test]
fn test_read_your_own_writes_analytic_scan() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    for t in ["r", "c"] {
        server
            .execute(&format!("CREATE TABLE {t} (id BIGINT PRIMARY KEY, v INT);"))
            .unwrap();
        server
            .execute(&format!(
                "INSERT INTO {t} (id, v) VALUES (1, 10), (2, 20), (3, 30);"
            ))
            .unwrap();
    }
    assert!(server.convert_table_to_column("c").unwrap().is_success());

    let mut session = server.open_session();
    session.begin().unwrap();
    for t in ["r", "c"] {
        session
            .execute(&format!("UPDATE {t} SET v = 999 WHERE id = 2;"))
            .unwrap();
        session
            .execute(&format!("INSERT INTO {t} (id, v) VALUES (4, 40);"))
            .unwrap();
    }

    for t in ["r", "c"] {
        // `v > 15` is a pushdown-eligible comparison leaf; the narrow analytic scan path must
        // still reflect the session's own uncommitted writes on both the Row and the Column
        // table.
        let got = session_rows(
            &mut session,
            &format!("SELECT id, v FROM {t} WHERE v > 15 ORDER BY id;"),
        );
        assert_eq!(
            got,
            vec![
                Row::new(vec![Value::Int64(2), Value::Int32(999)]),
                Row::new(vec![Value::Int64(3), Value::Int32(30)]),
                Row::new(vec![Value::Int64(4), Value::Int32(40)]),
            ],
            "table {t}"
        );
    }

    for t in ["r", "c"] {
        let outside = exec_rows(
            &server,
            &format!("SELECT id, v FROM {t} WHERE v > 15 ORDER BY id;"),
        );
        assert_eq!(
            outside,
            vec![
                Row::new(vec![Value::Int64(2), Value::Int32(20)]),
                Row::new(vec![Value::Int64(3), Value::Int32(30)]),
            ],
            "table {t}"
        );
    }
}

/// Storage-reviewer-requested test: a buffered write overlaid onto a `Column`-storage partition
/// scan (Phase 10 task 5) that does *not* satisfy a pushdown-eligible predicate must not appear
/// in the result. `overlay_rows` (`htap_server::session`) injects every buffered `Put` below
/// relational operators unconditionally, relying on `olap::execute_analytic_select_compact`'s
/// residual global filter (step 4 of `execute_analytic_select`) to re-apply the predicate over
/// the merged stream regardless of what the storage-level pushdown already pruned; this pins that
/// down as a real invariant, not an accident of `test_read_your_own_writes_analytic_scan`'s
/// buffered rows happening to all match.
#[test]
fn test_overlay_on_column_partition_excludes_buffered_row_not_matching_pushdown_predicate() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute("CREATE TABLE c (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("INSERT INTO c (id, v) VALUES (1, 10), (2, 20), (3, 30);")
        .unwrap();
    assert!(server.convert_table_to_column("c").unwrap().is_success());

    let mut session = server.open_session();
    session.begin().unwrap();
    // Buffered UPDATE: id=2's new value no longer satisfies `v > 15`.
    session.execute("UPDATE c SET v = 5 WHERE id = 2;").unwrap();
    // Buffered INSERT: a brand new row that also does not satisfy `v > 15`.
    session
        .execute("INSERT INTO c (id, v) VALUES (4, 1);")
        .unwrap();

    let got = session_rows(
        &mut session,
        "SELECT id, v FROM c WHERE v > 15 ORDER BY id;",
    );
    assert_eq!(
        got,
        vec![Row::new(vec![Value::Int64(3), Value::Int32(30)])],
        "buffered rows that do not satisfy the pushdown predicate must not appear"
    );

    // The buffered rows are visible without the predicate, confirming they really were buffered
    // (and not, say, silently dropped by some unrelated bug) and it is specifically the predicate
    // that excludes them above.
    let got_unfiltered = session_rows(&mut session, "SELECT id, v FROM c ORDER BY id;");
    assert_eq!(
        got_unfiltered,
        vec![
            Row::new(vec![Value::Int64(1), Value::Int32(10)]),
            Row::new(vec![Value::Int64(2), Value::Int32(5)]),
            Row::new(vec![Value::Int64(3), Value::Int32(30)]),
            Row::new(vec![Value::Int64(4), Value::Int32(1)]),
        ]
    );
    session.rollback().unwrap();
}

#[test]
fn test_read_your_own_writes_general_query_join() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute("CREATE TABLE a (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("CREATE TABLE b (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("INSERT INTO a (id, v) VALUES (1, 10), (2, 20);")
        .unwrap();
    server
        .execute("INSERT INTO b (id, v) VALUES (1, 100), (2, 200);")
        .unwrap();

    let mut session = server.open_session();
    session.begin().unwrap();
    session
        .execute("INSERT INTO a (id, v) VALUES (3, 30);")
        .unwrap();
    session
        .execute("INSERT INTO b (id, v) VALUES (3, 300);")
        .unwrap();
    session
        .execute("UPDATE b SET v = 999 WHERE id = 1;")
        .unwrap();

    let got = session_rows(
        &mut session,
        "SELECT a.id, a.v, b.v FROM a JOIN b ON a.id = b.id ORDER BY a.id;",
    );
    assert_eq!(
        got,
        vec![
            Row::new(vec![Value::Int64(1), Value::Int32(10), Value::Int32(999)]),
            Row::new(vec![Value::Int64(2), Value::Int32(20), Value::Int32(200)]),
            Row::new(vec![Value::Int64(3), Value::Int32(30), Value::Int32(300)]),
        ]
    );

    let outside = exec_rows(
        &server,
        "SELECT a.id, a.v, b.v FROM a JOIN b ON a.id = b.id ORDER BY a.id;",
    );
    assert_eq!(
        outside,
        vec![
            Row::new(vec![Value::Int64(1), Value::Int32(10), Value::Int32(100)]),
            Row::new(vec![Value::Int64(2), Value::Int32(20), Value::Int32(200)]),
        ]
    );
}

#[test]
fn test_double_update_in_one_transaction_composes() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("INSERT INTO t (id, v) VALUES (1, 1);")
        .unwrap();

    let mut session = server.open_session();
    session.begin().unwrap();
    session
        .execute("UPDATE t SET v = v + 10 WHERE id = 1;")
        .unwrap();
    session
        .execute("UPDATE t SET v = v + 10 WHERE id = 1;")
        .unwrap();

    assert_eq!(
        session_rows(&mut session, "SELECT v FROM t WHERE id = 1;"),
        vec![Row::new(vec![Value::Int32(21)])]
    );
    assert_eq!(
        exec_rows(&server, "SELECT v FROM t WHERE id = 1;"),
        vec![Row::new(vec![Value::Int32(1)])]
    );
}

#[test]
fn test_insert_then_delete_in_one_transaction_nets_to_nothing() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();

    let mut session = server.open_session();
    session.begin().unwrap();
    session
        .execute("INSERT INTO t (id, v) VALUES (5, 50);")
        .unwrap();
    session.execute("DELETE FROM t WHERE id = 5;").unwrap();

    assert!(session_rows(&mut session, "SELECT id FROM t WHERE id = 5;").is_empty());
    assert!(session_rows(&mut session, "SELECT id FROM t;").is_empty());
}

/// Storage-reviewer finding F6: a duplicate `(partition, key)` within a single `INSERT`
/// statement's own row batch must be rejected the same way in both modes. Autocommit already
/// rejects it (`Engine::prepare`'s duplicate-key check runs before anything commits); inside an
/// explicit transaction, buffering the batch into `WriteSet` (keyed by `(partition_id, key)`)
/// would otherwise silently keep only the last row instead — accepting a statement autocommit
/// would have rejected outright.
#[test]
fn test_duplicate_pk_within_one_insert_statement_rejected_in_transaction() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();

    // Autocommit rejects it outright.
    let autocommit_err = server
        .execute("INSERT INTO t (id, v) VALUES (1, 10), (1, 20);")
        .unwrap_err();
    assert!(matches!(autocommit_err, HtapError::InvalidArgument(_)));
    assert!(exec_rows(&server, "SELECT id FROM t;").is_empty());

    // An explicit transaction must reject the very same statement, not silently keep the last
    // row, and must not poison the transaction (this is an ordinary statement error).
    let mut session = server.open_session();
    session.begin().unwrap();
    let txn_err = session
        .execute("INSERT INTO t (id, v) VALUES (1, 10), (1, 20);")
        .unwrap_err();
    assert!(matches!(txn_err, HtapError::InvalidArgument(_)));
    assert!(
        session_rows(&mut session, "SELECT id FROM t;").is_empty(),
        "the rejected statement must not have buffered anything"
    );
    // The transaction itself survives (not poisoned) and can still commit other writes.
    session
        .execute("INSERT INTO t (id, v) VALUES (2, 200);")
        .unwrap();
    session.commit().unwrap();
    assert_eq!(
        exec_rows(&server, "SELECT id FROM t;"),
        vec![Row::new(vec![Value::Int64(2)])]
    );
}

#[test]
fn test_statement_failure_does_not_pollute_write_set() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT NOT NULL);")
        .unwrap();
    server
        .execute("INSERT INTO t (id, v) VALUES (1, 1), (2, 2);")
        .unwrap();

    let mut session = server.open_session();
    session.begin().unwrap();

    // A successful buffered write before the failing statement.
    session
        .execute("UPDATE t SET v = 100 WHERE id = 1;")
        .unwrap();

    // Violates NOT NULL; must not touch the write set at all.
    let err = session
        .execute("UPDATE t SET v = NULL WHERE id = 2;")
        .unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));

    // The earlier successful write survives...
    assert_eq!(
        session_rows(&mut session, "SELECT v FROM t WHERE id = 1;"),
        vec![Row::new(vec![Value::Int32(100)])]
    );
    // ...and the failed statement's target row is untouched (original value, not NULL).
    assert_eq!(
        session_rows(&mut session, "SELECT v FROM t WHERE id = 2;"),
        vec![Row::new(vec![Value::Int32(2)])]
    );
}

#[test]
fn test_read_your_own_writes_across_partitions() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute(
            "CREATE TABLE sales (id BIGINT PRIMARY KEY, amount INT) \
             PARTITION BY RANGE (id) ( \
                 PARTITION p0 VALUES LESS THAN (100), \
                 PARTITION p1 VALUES LESS THAN MAXVALUE \
             );",
        )
        .unwrap();
    server
        .execute("INSERT INTO sales (id, amount) VALUES (1, 10), (150, 20);")
        .unwrap();

    let mut session = server.open_session();
    session.begin().unwrap();
    // One buffered write per partition: an update in p0, an insert in p1.
    session
        .execute("UPDATE sales SET amount = 999 WHERE id = 1;")
        .unwrap();
    session
        .execute("INSERT INTO sales (id, amount) VALUES (200, 30);")
        .unwrap();

    assert_eq!(
        session_rows(&mut session, "SELECT amount FROM sales WHERE id = 1;"),
        vec![Row::new(vec![Value::Int32(999)])]
    );
    assert_eq!(
        session_rows(&mut session, "SELECT amount FROM sales WHERE id = 200;"),
        vec![Row::new(vec![Value::Int32(30)])]
    );

    // Full scan across both partitions, in ascending primary-key order.
    assert_eq!(
        session_rows(&mut session, "SELECT id, amount FROM sales ORDER BY id;"),
        vec![
            Row::new(vec![Value::Int64(1), Value::Int32(999)]),
            Row::new(vec![Value::Int64(150), Value::Int32(20)]),
            Row::new(vec![Value::Int64(200), Value::Int32(30)]),
        ]
    );

    // A pruned scan touching only p0 (`id < 100`) still sees that partition's buffered write.
    assert_eq!(
        session_rows(&mut session, "SELECT id, amount FROM sales WHERE id < 100;"),
        vec![Row::new(vec![Value::Int64(1), Value::Int32(999)])]
    );

    // Autocommit reads are unaffected.
    assert_eq!(
        exec_rows(&server, "SELECT id, amount FROM sales ORDER BY id;"),
        vec![
            Row::new(vec![Value::Int64(1), Value::Int32(10)]),
            Row::new(vec![Value::Int64(150), Value::Int32(20)]),
        ]
    );
}

// ---------------------------------------------------------------------------------------------
// Phase 10 task 6: COMMIT/ROLLBACK semantics, poisoning, catalog revalidation, DurablePending.
// ---------------------------------------------------------------------------------------------

#[test]
fn test_commit_flushes_buffered_writes_as_one_version() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();

    let mut session = server.open_session();
    session.begin().unwrap();
    session
        .execute("INSERT INTO t (id, v) VALUES (1, 10);")
        .unwrap();
    session
        .execute("INSERT INTO t (id, v) VALUES (2, 20);")
        .unwrap();
    session
        .execute("INSERT INTO t (id, v) VALUES (3, 30);")
        .unwrap();

    let before = server.txn_manager().next_version();
    session.commit().unwrap();
    let after = server.txn_manager().next_version();
    assert_eq!(
        after.get(),
        before.get() + 1,
        "three buffered mutations must flush as exactly one committed version"
    );

    assert_eq!(
        exec_rows(&server, "SELECT id, v FROM t ORDER BY id;"),
        vec![
            Row::new(vec![Value::Int64(1), Value::Int32(10)]),
            Row::new(vec![Value::Int64(2), Value::Int32(20)]),
            Row::new(vec![Value::Int64(3), Value::Int32(30)]),
        ]
    );
}

#[test]
fn test_commit_write_write_conflict_returns_clean_conflict_and_poisons_session() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("INSERT INTO t (id, v) VALUES (1, 1);")
        .unwrap();

    let mut session = server.open_session();
    session.begin().unwrap();
    session.execute("UPDATE t SET v = 2 WHERE id = 1;").unwrap();

    // A concurrent autocommit writer commits the same key after the session's snapshot.
    server.execute("UPDATE t SET v = 99 WHERE id = 1;").unwrap();

    let err = session.commit().unwrap_err();
    assert!(
        matches!(err, HtapError::Conflict(_)),
        "expected a clean Conflict, got {err:?}"
    );
    assert!(!err.is_durable_pending());

    // The session's transaction is gone (aborted): it is not stuck, a fresh transaction works.
    assert!(!session.in_transaction());
    session.begin().unwrap();
    session
        .execute("INSERT INTO t (id, v) VALUES (2, 2);")
        .unwrap();
    session.commit().unwrap();

    // The losing session's update never applied; the winner's value stands.
    assert_eq!(
        exec_rows(&server, "SELECT v FROM t WHERE id = 1;"),
        vec![Row::new(vec![Value::Int32(99)])]
    );
}

#[test]
fn test_commit_catalog_conflict_on_concurrent_drop_table() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("INSERT INTO t (id, v) VALUES (1, 1);")
        .unwrap();

    let mut session = server.open_session();
    session.begin().unwrap();
    session.execute("UPDATE t SET v = 2 WHERE id = 1;").unwrap();

    server.execute("DROP TABLE t;").unwrap();

    let err = session.commit().unwrap_err();
    assert!(
        matches!(err, HtapError::Conflict(_)),
        "expected a Conflict for a concurrently dropped table, got {err:?}"
    );
    assert!(!session.in_transaction());
}

#[test]
fn test_stale_snapshot_vs_conversion_returns_conflict_and_poisons_session() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("INSERT INTO t (id, v) VALUES (1, 10), (2, 20);")
        .unwrap();

    let mut session = server.open_session();
    session.begin().unwrap();

    // Advance the visible version past the session's pinned snapshot, then convert to columnar
    // storage: the columnar base's `base_version` is now strictly greater than the session's
    // snapshot.
    server
        .execute("INSERT INTO t (id, v) VALUES (3, 30);")
        .unwrap();
    assert!(server.convert_table_to_column("t").unwrap().is_success());

    let err = session
        .execute("SELECT id, v FROM t ORDER BY id;")
        .unwrap_err();
    assert!(
        matches!(err, HtapError::Conflict(_)),
        "expected a Conflict for a snapshot that predates the columnar base, got {err:?}"
    );

    // The transaction is poisoned: every further statement fails with the same stored message,
    // including a plain read that would otherwise succeed.
    let poisoned_msg = err.to_string();
    let err2 = session.execute("SELECT 1;").unwrap_err();
    assert_eq!(err2.to_string(), poisoned_msg);

    // COMMIT also fails with the stored conflict, and clears the transaction (aborted) rather
    // than leaving it stuck.
    let commit_err = session.commit().unwrap_err();
    assert_eq!(commit_err.to_string(), poisoned_msg);
    assert!(!session.in_transaction());
}

/// Storage-reviewer-requested test: a Row -> Column -> Row demotion cycle happening entirely
/// while a session transaction stays open across it. At every point, a read inside that
/// transaction must either return exactly the data that was actually committed at the
/// transaction's own pinned snapshot, or raise `Conflict` (task 6a's stale-snapshot-vs-columnar-
/// base check) — never silently wrong data from whichever storage format the partition happens
/// to be in at read time.
#[test]
fn test_row_column_row_demotion_cycle_during_open_transaction_stays_correct_or_conflicts() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("INSERT INTO t (id, v) VALUES (1, 10), (2, 20);")
        .unwrap();

    let mut session = server.open_session();
    session.begin().unwrap();

    // Baseline: correct as of the transaction's own snapshot, storage still Row.
    assert_eq!(
        session_rows(&mut session, "SELECT id, v FROM t ORDER BY id;"),
        vec![
            Row::new(vec![Value::Int64(1), Value::Int32(10)]),
            Row::new(vec![Value::Int64(2), Value::Int32(20)]),
        ]
    );

    // Advance the visible version past the session's pinned snapshot (exactly like
    // `test_stale_snapshot_vs_conversion_returns_conflict_and_poisons_session`), then convert to
    // columnar storage: the columnar base's `base_version` is now strictly greater than the
    // still-open transaction's pinned snapshot.
    server
        .execute("INSERT INTO t (id, v) VALUES (3, 30);")
        .unwrap();
    assert!(server.convert_table_to_column("t").unwrap().is_success());

    // The transaction's stale snapshot vs. the columnar base is a clean `Conflict`, never wrong
    // data, and poisons the transaction.
    let err = session
        .execute("SELECT id, v FROM t ORDER BY id;")
        .unwrap_err();
    assert!(
        matches!(err, HtapError::Conflict(_)),
        "expected Conflict for a snapshot predating the columnar base, got {err:?}"
    );

    // Demote back to row storage (still outside the transaction, which stays open, poisoned).
    assert!(server.convert_table_to_row("t").unwrap().is_success());

    // The poisoned transaction still fails every further statement with the same stored
    // message — not silently "fixed" by the demotion, and not wrong data either.
    let err_after_demotion = session
        .execute("SELECT id, v FROM t ORDER BY id;")
        .unwrap_err();
    assert_eq!(err_after_demotion.to_string(), err.to_string());

    // Rolling back and starting a fresh transaction against the now-Row-again table returns
    // exactly the correct, current data (including the row committed while the old transaction
    // was open).
    session.rollback().unwrap();
    session.begin().unwrap();
    assert_eq!(
        session_rows(&mut session, "SELECT id, v FROM t ORDER BY id;"),
        vec![
            Row::new(vec![Value::Int64(1), Value::Int32(10)]),
            Row::new(vec![Value::Int64(2), Value::Int32(20)]),
            Row::new(vec![Value::Int64(3), Value::Int32(30)]),
        ]
    );
    session.rollback().unwrap();
}

/// Storage-reviewer-requested test: a `Converting` partition still in the `SnapshotPinned` phase
/// (no column manifest written yet) read inside an open session transaction. Reached the same
/// way `local_server.rs`'s `test_convert_table_multi_partition_guard`-adjacent tests reach it —
/// there is no public server API to pause a conversion mid-flight at exactly this phase
/// deterministically, so the catalog is edited directly through `LocalCatalogStore`'s CAS, the
/// same technique already used elsewhere in this test suite (see `local_server.rs`) to reach this
/// state; in production it is reached transiently by `LocalConverter::convert_partition` between
/// pinning the source snapshot and finishing writing segments. `scan_partition_compact`'s
/// `Converting` branch has no manifest to compare a snapshot against in this phase, so it always
/// falls back straight to the rowstore (never the task 6a stale-vs-columnar-base conflict, since
/// there is no columnar base yet) and must return exactly the correct data.
#[test]
fn test_snapshot_pinned_converting_partition_without_manifest_during_open_transaction() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("INSERT INTO t (id, v) VALUES (1, 10), (2, 20);")
        .unwrap();

    let mut session = server.open_session();
    session.begin().unwrap();

    let catalog_store = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
    let mut snap = catalog_store.load().unwrap().unwrap();
    let expected_generation = snap.generation;
    let conv_gen = snap.generation + 1;
    snap.generation = conv_gen;
    snap.partitions[0].generation = conv_gen;
    snap.partitions[0].storage = StorageDescriptor::Converting {
        from: StorageFormat::Row,
        to: StorageFormat::Column,
        generation: conv_gen,
    };
    snap.partitions[0].conversion = Some(ConversionDescriptor::new(
        conv_gen,
        StorageFormat::Row,
        StorageFormat::Column,
        Version::new(3),
        ConversionPhase::SnapshotPinned,
    ));
    catalog_store
        .compare_and_set(expected_generation, snap)
        .unwrap();

    // No column manifest exists on disk for this tablet yet (`SnapshotPinned`, before segments
    // are written), so the read must fall back to the rowstore and return the correct data —
    // never a `Conflict` (there is no columnar base to be stale against yet) and never wrong
    // data.
    assert_eq!(
        session_rows(&mut session, "SELECT id, v FROM t ORDER BY id;"),
        vec![
            Row::new(vec![Value::Int64(1), Value::Int32(10)]),
            Row::new(vec![Value::Int64(2), Value::Int32(20)]),
        ]
    );
    assert!(
        session.in_transaction(),
        "no conflict means the transaction is not poisoned and stays open"
    );
    session.rollback().unwrap();
}

#[test]
fn test_rollback_after_poison_succeeds() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("INSERT INTO t (id, v) VALUES (1, 10);")
        .unwrap();

    let mut session = server.open_session();
    session.begin().unwrap();
    server
        .execute("INSERT INTO t (id, v) VALUES (2, 20);")
        .unwrap();
    assert!(server.convert_table_to_column("t").unwrap().is_success());

    let err = session
        .execute("SELECT id, v FROM t ORDER BY id;")
        .unwrap_err();
    assert!(matches!(err, HtapError::Conflict(_)));

    session.rollback().unwrap();
    assert!(!session.in_transaction());

    // The session is fully usable again.
    session.begin().unwrap();
    session
        .execute("INSERT INTO t (id, v) VALUES (3, 30);")
        .unwrap();
    session.commit().unwrap();
    assert_eq!(
        exec_rows(&server, "SELECT id FROM t WHERE id = 3;"),
        vec![Row::new(vec![Value::Int64(3)])]
    );
}

#[test]
fn test_read_only_transaction_commit_skips_2pc() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("INSERT INTO t (id, v) VALUES (1, 10);")
        .unwrap();

    let mut session = server.open_session();
    session.execute("START TRANSACTION READ ONLY;").unwrap();
    assert_eq!(
        session_rows(&mut session, "SELECT v FROM t WHERE id = 1;"),
        vec![Row::new(vec![Value::Int32(10)])]
    );

    let before = server.txn_manager().next_version();
    session.execute("COMMIT;").unwrap();
    let after = server.txn_manager().next_version();
    assert_eq!(
        after, before,
        "a read-only transaction's commit must not allocate a version"
    );
    assert!(!session.in_transaction());
}

#[test]
fn test_durable_pending_from_commit_leaves_session_in_outcome_pending_state() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();

    let mut session = server.open_session();
    session.begin().unwrap();
    session
        .execute("INSERT INTO t (id, v) VALUES (1, 1);")
        .unwrap();

    server.txn_manager().set_commit_append_hook(|_journal| {
        Err(HtapError::Io(std::io::Error::other(
            "simulated disk failure during commit record append",
        )))
    });

    let err = session.commit().unwrap_err();
    assert!(
        err.is_durable_pending(),
        "expected DurablePending, got {err:?}"
    );

    // Every further statement, including ROLLBACK, is rejected with the original
    // `DurablePending` error, never `HtapError::Conflict` (storage-reviewer finding F1):
    // `Conflict` maps to MySQL 1213/`40001` ("rolled back, retry"), which would invite a client
    // to double-apply this write. `DurablePending` maps to 1105 instead (see
    // `htap-wire/src/error_map.rs`).
    let rollback_err = session.rollback().unwrap_err();
    assert!(
        rollback_err.is_durable_pending(),
        "expected DurablePending, got {rollback_err:?}"
    );
    assert!(!matches!(rollback_err, HtapError::Conflict(_)));
    assert!(rollback_err
        .to_string()
        .to_ascii_lowercase()
        .contains("recovery"));

    let exec_err = session.execute("SELECT 1;").unwrap_err();
    assert!(
        exec_err.is_durable_pending(),
        "expected DurablePending, got {exec_err:?}"
    );
    assert!(!matches!(exec_err, HtapError::Conflict(_)));
    assert!(exec_err
        .to_string()
        .to_ascii_lowercase()
        .contains("recovery"));
}

/// Fix-pass item 5: a session commit rejected only because the manager is latched behind an
/// *unrelated* transaction's still-ambiguous commit must not adopt that other transaction's own
/// `DurablePending` as its outcome (this session's own commit never even attempted anything) —
/// it gets `HtapError::RecoveryRequired` instead, and its own transaction stays open with its
/// write set intact, rather than being quarantined into `CommitOutcomePending` for a recovery
/// this session has nothing to do with.
#[test]
fn test_commit_while_manager_latched_by_other_session_keeps_txn_open() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();

    // Session A's commit fails at the journal decision boundary, latching the manager.
    let mut session_a = server.open_session();
    session_a.begin().unwrap();
    session_a
        .execute("INSERT INTO t (id, v) VALUES (1, 1);")
        .unwrap();
    server.txn_manager().set_commit_append_hook(|_journal| {
        Err(HtapError::Io(std::io::Error::other(
            "simulated disk failure during commit record append",
        )))
    });
    let err_a = session_a.commit().unwrap_err();
    assert!(
        err_a.is_durable_pending(),
        "expected DurablePending for A, got {err_a:?}"
    );

    // Session B is unrelated and still has its own open transaction with a buffered write. Its
    // commit must fail with `RecoveryRequired` naming A, never `Conflict` and never A's own
    // `DurablePending`.
    let mut session_b = server.open_session();
    session_b.begin().unwrap();
    session_b
        .execute("INSERT INTO t (id, v) VALUES (2, 2);")
        .unwrap();
    let err_b = session_b.commit().unwrap_err();
    assert!(
        err_b.is_recovery_required(),
        "expected RecoveryRequired for B, got {err_b:?}"
    );
    assert!(!matches!(err_b, HtapError::Conflict(_)));
    assert!(
        !err_b.is_durable_pending(),
        "B must not adopt A's own DurablePending outcome, got {err_b:?}"
    );

    // B's own transaction stays open with its write set intact: its buffered write is still
    // readable, and B is not quarantined the way A is.
    assert!(
        session_b.in_transaction(),
        "B's transaction must stay open, since B's own commit never attempted anything"
    );
    assert_eq!(
        session_rows(&mut session_b, "SELECT v FROM t WHERE id = 2;"),
        vec![Row::new(vec![Value::Int32(2)])]
    );
    session_b
        .rollback()
        .expect("B is not quarantined and can roll back normally");
    assert!(!session_b.in_transaction());
}

/// Fix-pass item 3: a single buffered row whose own mutation payload comfortably fits under the
/// 16 MiB `RowstoreParticipant::encode_payload` cap can still produce a durable `Intent` journal
/// frame bigger than the journal's `max_frame_size`, once `serde_json` re-encodes the payload as
/// a JSON number array (`intent_frame_size_bound`). This must be rejected with `InvalidArgument`
/// — not `DurablePending`, and not by silently discarding the transaction — and the transaction
/// must stay open (F7 semantics), exactly like the pre-existing 16 MiB cap.
#[test]
fn test_commit_of_write_set_exceeding_intent_frame_is_rejected_and_txn_stays_open() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute("CREATE TABLE big (id BIGINT PRIMARY KEY, data VARCHAR);")
        .unwrap();

    let mut session = server.open_session();
    session.begin().unwrap();

    // ~4.5 MiB of raw string data: well under the 16 MiB `MAX_PAYLOAD_SIZE`, but its estimated
    // durable Intent frame (roughly 4x once JSON-number-array-encoded, per
    // `intent_frame_size_bound`'s doc) exceeds `DEFAULT_MAX_FRAME_SIZE` (16 MiB).
    let big_value = "x".repeat(4_500_000);
    let err = session
        .execute(&format!(
            "INSERT INTO big (id, data) VALUES (1, '{big_value}');"
        ))
        .unwrap_err();
    assert!(
        matches!(err, HtapError::InvalidArgument(_)),
        "expected InvalidArgument, got {err:?}"
    );
    assert!(!err.is_durable_pending());

    // The write set was never polluted, and the transaction stays open: a normal statement and
    // commit afterward both work cleanly.
    assert!(session.in_transaction());
    session
        .execute("INSERT INTO big (id, data) VALUES (2, 'small');")
        .unwrap();
    session.commit().unwrap();
    assert_eq!(
        exec_rows(&server, "SELECT id FROM big;"),
        vec![Row::new(vec![Value::Int64(2)])]
    );
}

/// Fix-pass item 3: the same oversize-Intent-frame rejection applies to an autocommit write
/// (never buffered in any `WriteSet`), and it must be rejected cleanly before any journal record
/// is written — `TransactionManager::commit` itself checks this before prepare (item 3c).
#[test]
fn test_autocommit_oversize_insert_rejected_cleanly_before_any_journal_write() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute("CREATE TABLE big (id BIGINT PRIMARY KEY, data VARCHAR);")
        .unwrap();

    let journal_path = dir.path().join("txn.journal");
    let journal_len_before = std::fs::metadata(&journal_path).unwrap().len();

    let big_value = "x".repeat(4_500_000);
    let sql = format!("INSERT INTO big (id, data) VALUES (1, '{big_value}');");
    let err = server.execute(&sql).unwrap_err();
    assert!(
        matches!(err, HtapError::InvalidArgument(_)),
        "expected InvalidArgument, got {err:?}"
    );
    assert!(!err.is_durable_pending());

    let journal_len_after = std::fs::metadata(&journal_path).unwrap().len();
    assert_eq!(
        journal_len_before, journal_len_after,
        "an oversize autocommit write must be rejected before any journal record is written"
    );
    assert!(exec_rows(&server, "SELECT id FROM big;").is_empty());

    // The manager's own state was untouched: a normal statement afterward still works cleanly.
    server
        .execute("INSERT INTO big (id, data) VALUES (2, 'small');")
        .unwrap();
    assert_eq!(
        exec_rows(&server, "SELECT id FROM big;"),
        vec![Row::new(vec![Value::Int64(2)])]
    );
}

// ---------------------------------------------------------------------------------------------
// Phase 10 task 7: control statements and the autocommit state machine.
// ---------------------------------------------------------------------------------------------

#[test]
fn test_autocommit_zero_starts_implicit_transaction() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();

    let mut session = server.open_session();
    session.execute("SET autocommit = 0;").unwrap();
    assert!(!session.in_transaction());

    session
        .execute("INSERT INTO t (id, v) VALUES (1, 10);")
        .unwrap();
    assert!(
        session.in_transaction(),
        "the first statement under autocommit=0 must implicitly begin a transaction"
    );
    assert!(exec_rows(&server, "SELECT id FROM t;").is_empty());

    session.execute("COMMIT;").unwrap();
    assert!(!session.in_transaction());
    assert_eq!(
        exec_rows(&server, "SELECT id FROM t;"),
        vec![Row::new(vec![Value::Int64(1)])]
    );
}

/// Storage-reviewer finding F8: an autocommit write dispatched through a `Session` (autocommit
/// stays on, so the statement never opens a transaction) that returns `DurablePending` must
/// quarantine the session exactly like a session `COMMIT` that returns `DurablePending` does —
/// not leave it `Idle`, where a later statement could run normally against a rowstore whose
/// recovery is still outstanding.
#[test]
fn test_autocommit_durable_pending_in_session_quarantines_session() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();

    let mut session = server.open_session();
    assert!(!session.in_transaction());

    server.txn_manager().set_commit_append_hook(|_journal| {
        Err(HtapError::Io(std::io::Error::other(
            "simulated disk failure during commit record append",
        )))
    });

    let err = session
        .execute("INSERT INTO t (id, v) VALUES (1, 1);")
        .unwrap_err();
    assert!(
        err.is_durable_pending(),
        "expected DurablePending, got {err:?}"
    );
    assert!(!session.in_transaction());

    // The session is quarantined: every further statement, including one that would otherwise
    // succeed trivially, is rejected with the same stored `DurablePending`, never `Ok`.
    let exec_err = session.execute("SELECT 1;").unwrap_err();
    assert!(
        exec_err.is_durable_pending(),
        "expected DurablePending, got {exec_err:?}"
    );
    let begin_err = session.begin().unwrap_err();
    assert!(begin_err.is_durable_pending());
}

#[test]
fn test_begin_while_active_implicitly_commits_previous() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();

    let mut session = server.open_session();
    session.execute("BEGIN;").unwrap();
    session
        .execute("INSERT INTO t (id, v) VALUES (1, 10);")
        .unwrap();

    // A second BEGIN implicitly commits the first transaction before opening a new one.
    session.execute("BEGIN;").unwrap();
    assert_eq!(
        exec_rows(&server, "SELECT id FROM t;"),
        vec![Row::new(vec![Value::Int64(1)])]
    );

    session
        .execute("INSERT INTO t (id, v) VALUES (2, 20);")
        .unwrap();
    assert_eq!(
        exec_rows(&server, "SELECT id FROM t;"),
        vec![Row::new(vec![Value::Int64(1)])],
        "the second transaction's write must still be buffered, not yet committed"
    );

    session.execute("COMMIT;").unwrap();
    assert_eq!(
        session_rows(&mut session, "SELECT id FROM t ORDER BY id;"),
        vec![
            Row::new(vec![Value::Int64(1)]),
            Row::new(vec![Value::Int64(2)]),
        ]
    );
}

#[test]
fn test_begin_implicit_commit_failure_does_not_start_new_txn() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("INSERT INTO t (id, v) VALUES (1, 1);")
        .unwrap();

    let mut session = server.open_session();
    session.execute("BEGIN;").unwrap();
    session.execute("UPDATE t SET v = 2 WHERE id = 1;").unwrap();

    // A concurrent autocommit writer invalidates the buffered update.
    server.execute("UPDATE t SET v = 99 WHERE id = 1;").unwrap();

    let err = session.execute("BEGIN;").unwrap_err();
    assert!(matches!(err, HtapError::Conflict(_)));
    assert!(
        !session.in_transaction(),
        "a failed implicit commit must not leave a new transaction open"
    );
}

/// Storage-reviewer finding F2: the public `Session::begin()` API must go through the same
/// guarded path as SQL `BEGIN` — rejected outright once the session is `CommitOutcomePending`,
/// never silently overwriting that state so a later `rollback()` would wrongly report `Ok`.
#[test]
fn test_begin_api_after_outcome_pending_is_rejected() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();

    let mut session = server.open_session();
    session.begin().unwrap();
    session
        .execute("INSERT INTO t (id, v) VALUES (1, 1);")
        .unwrap();

    server.txn_manager().set_commit_append_hook(|_journal| {
        Err(HtapError::Io(std::io::Error::other(
            "simulated disk failure during commit record append",
        )))
    });

    let err = session.commit().unwrap_err();
    assert!(err.is_durable_pending());

    // `begin()` must reject just like `execute("BEGIN")`/`rollback()` would, not silently
    // overwrite `CommitOutcomePending` with a fresh transaction.
    let begin_err = session.begin().unwrap_err();
    assert!(
        begin_err.is_durable_pending(),
        "expected DurablePending, got {begin_err:?}"
    );
    assert!(!session.in_transaction());

    // A subsequent `rollback()` must still report the same outcome-pending error, not `Ok`.
    let rollback_err = session.rollback().unwrap_err();
    assert!(rollback_err.is_durable_pending());
}

/// Storage-reviewer finding F2: calling the public `Session::begin()` API twice in a row must
/// implicitly commit the first transaction's buffered write set (matching SQL `BEGIN; BEGIN;`),
/// never silently drop it.
#[test]
fn test_begin_api_twice_implicitly_commits_not_drops() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();

    let mut session = server.open_session();
    session.begin().unwrap();
    session
        .execute("INSERT INTO t (id, v) VALUES (1, 10);")
        .unwrap();

    // Calling `begin()` again must implicitly commit the first write, not drop it.
    session.begin().unwrap();
    assert_eq!(
        exec_rows(&server, "SELECT id FROM t;"),
        vec![Row::new(vec![Value::Int64(1)])],
        "the first transaction's buffered write must have been committed, not dropped"
    );

    session
        .execute("INSERT INTO t (id, v) VALUES (2, 20);")
        .unwrap();
    session.commit().unwrap();
    assert_eq!(
        session_rows(&mut session, "SELECT id FROM t ORDER BY id;"),
        vec![
            Row::new(vec![Value::Int64(1)]),
            Row::new(vec![Value::Int64(2)]),
        ]
    );
}

#[test]
fn test_ddl_rejected_inside_explicit_transaction_and_txn_survives() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();

    let mut session = server.open_session();
    session.execute("BEGIN;").unwrap();
    session
        .execute("INSERT INTO t (id, v) VALUES (1, 1);")
        .unwrap();

    let err = session
        .execute("CREATE TABLE x (id BIGINT PRIMARY KEY);")
        .unwrap_err();
    assert!(matches!(err, HtapError::Unsupported(_)));
    assert!(session.in_transaction(), "the transaction must survive");

    // The transaction is not poisoned: further statements and COMMIT still work.
    session
        .execute("INSERT INTO t (id, v) VALUES (2, 2);")
        .unwrap();
    session.commit().unwrap();
    assert_eq!(
        exec_rows(&server, "SELECT id FROM t ORDER BY id;"),
        vec![
            Row::new(vec![Value::Int64(1)]),
            Row::new(vec![Value::Int64(2)]),
        ]
    );
    assert!(catalog_has_no_table(&server, "x"));
}

fn catalog_has_no_table(server: &LocalServer, name: &str) -> bool {
    exec_rows(server, "SHOW TABLES;")
        .into_iter()
        .all(|row| row.get(0) != Some(&Value::String(name.to_string())))
}

#[test]
fn test_set_user_variable_and_select_it_back() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    let mut session = server.open_session();

    session.execute("SET @x = 42;").unwrap();
    assert_eq!(
        session_rows(&mut session, "SELECT @x;"),
        vec![Row::new(vec![Value::Int64(42)])]
    );

    // Multiple comma-separated assignments, the second referencing the first through this same
    // session's `VariableLookup`.
    session.execute("SET @a = 1, @b = @a + 1;").unwrap();
    assert_eq!(
        session_rows(&mut session, "SELECT @a, @b;"),
        vec![Row::new(vec![Value::Int64(1), Value::Int64(2)])]
    );

    // An unset user variable reads back as NULL.
    assert_eq!(
        session_rows(&mut session, "SELECT @never_set;"),
        vec![Row::new(vec![Value::Null])]
    );
}

#[test]
fn test_set_autocommit_variable_matches_state() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    let mut session = server.open_session();

    assert_eq!(
        session_rows(&mut session, "SELECT @@autocommit;"),
        vec![Row::new(vec![Value::Int64(1)])]
    );

    session.execute("SET autocommit = 0;").unwrap();
    assert_eq!(
        session_rows(&mut session, "SELECT @@autocommit;"),
        vec![Row::new(vec![Value::Int64(0)])]
    );

    session.execute("SET autocommit = 1;").unwrap();
    assert_eq!(
        session_rows(&mut session, "SELECT @@autocommit;"),
        vec![Row::new(vec![Value::Int64(1)])]
    );
}

#[test]
fn test_set_autocommit_one_commits_active_transaction() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();

    let mut session = server.open_session();
    session.execute("SET autocommit = 0;").unwrap();
    session
        .execute("INSERT INTO t (id, v) VALUES (1, 1);")
        .unwrap();
    assert!(session.in_transaction());
    assert!(exec_rows(&server, "SELECT id FROM t;").is_empty());

    session.execute("SET autocommit = 1;").unwrap();
    assert!(!session.in_transaction());
    assert_eq!(
        exec_rows(&server, "SELECT id FROM t;"),
        vec![Row::new(vec![Value::Int64(1)])]
    );
}

#[test]
fn test_set_transaction_isolation_level_rejects_unsupported_level() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    let mut session = server.open_session();

    let err = session
        .execute("SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED;")
        .unwrap_err();
    assert!(matches!(err, HtapError::Unsupported(_)));

    session
        .execute("SET SESSION TRANSACTION ISOLATION LEVEL REPEATABLE READ;")
        .unwrap();

    let err2 = session
        .execute("BEGIN TRANSACTION ISOLATION LEVEL SERIALIZABLE;")
        .unwrap_err();
    assert!(matches!(err2, HtapError::Unsupported(_)));
    assert!(!session.in_transaction());
}

#[test]
fn test_set_transaction_read_only_rejects_write() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("INSERT INTO t (id, v) VALUES (1, 1);")
        .unwrap();

    let mut session = server.open_session();
    session.execute("SET TRANSACTION READ ONLY;").unwrap();
    session.execute("BEGIN;").unwrap();

    let err = session
        .execute("INSERT INTO t (id, v) VALUES (2, 2);")
        .unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));

    // Reads still work, and COMMIT succeeds (skipping 2PC; nothing was buffered).
    assert_eq!(
        session_rows(&mut session, "SELECT v FROM t WHERE id = 1;"),
        vec![Row::new(vec![Value::Int32(1)])]
    );
    session.execute("COMMIT;").unwrap();
}

#[test]
fn test_connector_startup_set_statements_accepted() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());
    let mut session = server.open_session();

    for sql in [
        "SET NAMES utf8mb4;",
        "SET NAMES utf8mb4 COLLATE utf8mb4_general_ci;",
        "SET character_set_results = utf8mb4;",
        "SET SESSION sql_mode = 'STRICT_TRANS_TABLES';",
        "SET time_zone = '+00:00';",
        "SET collation_connection = utf8mb4_general_ci;",
        "SET SESSION net_write_timeout = 60;",
        "SET autocommit = 1;",
    ] {
        session
            .execute(sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    let err = session
        .execute("SET some_totally_unknown_variable = 1;")
        .unwrap_err();
    assert!(matches!(err, HtapError::Unsupported(_)));

    let err2 = session.execute("SET GLOBAL autocommit = 1;").unwrap_err();
    assert!(matches!(err2, HtapError::Unsupported(_)));
}

#[test]
fn test_select_system_variable_via_plain_execute() {
    let dir = TempDir::new().unwrap();
    let server = Arc::new(LocalServer::open(dir.path()).unwrap());

    assert_eq!(
        exec_rows(&server, "SELECT @@autocommit;"),
        vec![Row::new(vec![Value::Int64(1)])]
    );
    // A user variable has nowhere to live outside a session: it reads as NULL, matching MySQL.
    assert_eq!(
        exec_rows(&server, "SELECT @x;"),
        vec![Row::new(vec![Value::Null])]
    );
}
