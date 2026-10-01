use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use htap_catalog::CatalogStore;
use htap_common::types::{Row, Value};
use htap_server::{LocalServer, Session};
use htap_sql::result::StatementResult;

static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(1);

struct TestServer {
    root: PathBuf,
    server: Arc<LocalServer>,
}

impl TestServer {
    fn open() -> Self {
        let unique_id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time must be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "htap-server-serializable-anomalies-{}-{timestamp}-{unique_id}",
            std::process::id()
        ));
        let server = Arc::new(LocalServer::open(&root).expect("server opens"));
        Self { root, server }
    }

    fn session(&self) -> Session {
        self.server.open_session().expect("session opens")
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.root) {
            if error.kind() != std::io::ErrorKind::NotFound {
                panic!(
                    "failed to remove test server directory {}: {error}",
                    self.root.display()
                );
            }
        }
    }
}

fn execute(session: &mut Session, sql: &str) {
    let statement = htap_sql::parse_one(sql).expect("SQL parses");
    session
        .execute_statement(statement)
        .unwrap_or_else(|error| panic!("SQL failed: {sql}: {error}"));
}

fn execute_error(session: &mut Session, sql: &str) -> String {
    let statement = htap_sql::parse_one(sql).expect("SQL parses");
    session
        .execute_statement(statement)
        .expect_err("statement must fail")
        .to_string()
}

fn query_rows(session: &mut Session, sql: &str) -> Vec<Row> {
    let statement = htap_sql::parse_one(sql).expect("SQL parses");
    match session
        .execute_statement(statement)
        .unwrap_or_else(|error| panic!("SQL failed: {sql}: {error}"))
    {
        StatementResult::Query(query) => query.rows,
        other => panic!("expected query result for {sql}, got {other:?}"),
    }
}

fn open_server_and_session() -> (TestServer, Session) {
    let server = TestServer::open();
    let session = server.session();
    (server, session)
}

fn create_table(session: &mut Session, table: &str) {
    execute(
        session,
        &format!("CREATE TABLE {table} (id INT PRIMARY KEY, value INT NOT NULL)"),
    );
}

fn create_partitioned_table(session: &mut Session, table: &str) {
    execute(
        session,
        &format!(
            "CREATE TABLE {table} (id INT PRIMARY KEY, value INT NOT NULL) \
             PARTITION BY RANGE (id) (\
                 PARTITION p0 VALUES LESS THAN (10), \
                 PARTITION p1 VALUES LESS THAN (20), \
                 PARTITION p2 VALUES LESS THAN MAXVALUE\
             )"
        ),
    );
}

fn begin_serializable(session: &mut Session) {
    execute(session, "START TRANSACTION ISOLATION LEVEL SERIALIZABLE");
}

fn begin_snapshot_isolation(session: &mut Session) {
    execute(session, "START TRANSACTION ISOLATION LEVEL REPEATABLE READ");
}

fn begin_mode(session: &mut Session, serializable: bool) {
    if serializable {
        begin_serializable(session);
    } else {
        begin_snapshot_isolation(session);
    }
}

fn assert_serialization_failure(error: &str) {
    let lower = error.to_ascii_lowercase();
    assert!(
        lower.contains("serialization failure") || lower.contains("read-write dependency"),
        "unexpected serialization error: {error}"
    );
}

fn run_write_skew_on_call_doctors(serializable: bool) {
    let (server, mut setup) = open_server_and_session();
    execute(
        &mut setup,
        "CREATE TABLE doctors (id INT PRIMARY KEY, on_call INT NOT NULL)",
    );
    execute(
        &mut setup,
        "INSERT INTO doctors (id, on_call) VALUES (1, 1), (2, 1)",
    );

    let mut t1 = server.session();
    let mut t2 = server.session();
    begin_mode(&mut t1, serializable);
    begin_mode(&mut t2, serializable);
    assert_eq!(query_rows(&mut t1, "SELECT * FROM doctors").len(), 2);
    assert_eq!(query_rows(&mut t2, "SELECT * FROM doctors").len(), 2);
    execute(&mut t1, "UPDATE doctors SET on_call = 0 WHERE id = 1");
    execute(&mut t2, "UPDATE doctors SET on_call = 0 WHERE id = 2");
    execute(&mut t1, "COMMIT");

    if serializable {
        assert_serialization_failure(&execute_error(&mut t2, "COMMIT"));
    } else {
        execute(&mut t2, "COMMIT");
    }

    // Invariant: at least one doctor must stay on call.
    let expected_on_call = if serializable { 1 } else { 0 };
    assert_eq!(
        query_rows(&mut setup, "SELECT COUNT(*) FROM doctors WHERE on_call = 1"),
        vec![Row::new(vec![Value::Int64(expected_on_call)])]
    );
}

#[test]
fn write_skew_on_call_doctors() {
    run_write_skew_on_call_doctors(false);
    run_write_skew_on_call_doctors(true);
}

fn run_read_only_anomaly(serializable: bool) {
    let (server, mut setup) = open_server_and_session();
    execute(
        &mut setup,
        "CREATE TABLE accounts (id INT PRIMARY KEY, balance INT NOT NULL)",
    );
    execute(
        &mut setup,
        "INSERT INTO accounts (id, balance) VALUES (1, 100), (2, 100)",
    );

    let mut withdraw = server.session();
    let mut deposit = server.session();
    let mut report = server.session();
    begin_mode(&mut withdraw, serializable);
    begin_mode(&mut deposit, serializable);

    assert_eq!(
        query_rows(
            &mut withdraw,
            "SELECT balance FROM accounts WHERE id IN (1, 2)"
        ),
        vec![
            Row::new(vec![Value::Int32(100)]),
            Row::new(vec![Value::Int32(100)]),
        ]
    );
    assert_eq!(
        query_rows(&mut deposit, "SELECT balance FROM accounts WHERE id = 2"),
        vec![Row::new(vec![Value::Int32(100)])]
    );

    execute(
        &mut deposit,
        "UPDATE accounts SET balance = balance + 100 WHERE id = 2",
    );
    execute(&mut deposit, "COMMIT");

    if serializable {
        execute(
            &mut report,
            "START TRANSACTION ISOLATION LEVEL SERIALIZABLE READ ONLY",
        );
    } else {
        execute(
            &mut report,
            "START TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY",
        );
    }
    assert_eq!(
        query_rows(&mut report, "SELECT balance FROM accounts WHERE id = 1"),
        vec![Row::new(vec![Value::Int32(100)])]
    );
    assert_eq!(
        query_rows(&mut report, "SELECT balance FROM accounts WHERE id = 2"),
        vec![Row::new(vec![Value::Int32(200)])]
    );

    execute(
        &mut withdraw,
        "UPDATE accounts SET balance = balance - 50 WHERE id = 1",
    );

    // Under SI, the report observes the Fekete/O'Neil/O'Neil read-only anomaly and both
    // writers commit. Under SERIALIZABLE, withdraw's read-write dependency on deposit aborts it;
    // the declared read-only report is never aborted.
    if serializable {
        assert_serialization_failure(&execute_error(&mut withdraw, "COMMIT"));
    } else {
        execute(&mut withdraw, "COMMIT");
    }
    execute(&mut report, "COMMIT");

    // Under SI, account 1 is 50 and account 2 is 200: the withdrawal committed from its stale
    // snapshot even though the read-only report observed the deposit but not the withdrawal.
    // Under SERIALIZABLE, the failed withdrawal leaves account 1 at its original balance of 100.
    let expected_account_1 = if serializable { 100 } else { 50 };
    assert_eq!(
        query_rows(&mut setup, "SELECT balance FROM accounts ORDER BY id"),
        vec![
            Row::new(vec![Value::Int32(expected_account_1)]),
            Row::new(vec![Value::Int32(200)]),
        ]
    );
}

#[test]
fn read_only_anomaly_fekete_deposit_withdraw_report() {
    run_read_only_anomaly(false);
    run_read_only_anomaly(true);
}

fn run_range_phantom(serializable: bool, columnar: bool) {
    let (server, mut setup) = open_server_and_session();
    execute(
        &mut setup,
        "CREATE TABLE range_rows (id INT PRIMARY KEY, amount INT NOT NULL)",
    );
    execute(
        &mut setup,
        "INSERT INTO range_rows (id, amount) VALUES (1, 150), (2, 150)",
    );
    if columnar {
        assert!(server
            .server
            .convert_table_to_column("range_rows")
            .expect("converts")
            .is_success());
    }

    let mut t1 = server.session();
    let mut t2 = server.session();
    begin_mode(&mut t1, serializable);
    begin_mode(&mut t2, serializable);
    for transaction in [&mut t1, &mut t2] {
        assert_eq!(
            query_rows(
                transaction,
                "SELECT COUNT(*) FROM range_rows WHERE amount < 100"
            ),
            vec![Row::new(vec![Value::Int64(0)])]
        );
    }
    execute(
        &mut t1,
        "INSERT INTO range_rows (id, amount) VALUES (3, 50)",
    );
    execute(
        &mut t2,
        "INSERT INTO range_rows (id, amount) VALUES (4, 50)",
    );
    execute(&mut t1, "COMMIT");
    if serializable {
        assert_serialization_failure(&execute_error(&mut t2, "COMMIT"));
    } else {
        execute(&mut t2, "COMMIT");
    }

    // Invariant: the range holds at most one row.
    let expected_count = if serializable { 1 } else { 2 };
    assert_eq!(
        query_rows(
            &mut setup,
            "SELECT COUNT(*) FROM range_rows WHERE amount < 100"
        ),
        vec![Row::new(vec![Value::Int64(expected_count)])]
    );
}

#[test]
fn phantom_write_skew_over_range_count_then_insert() {
    run_range_phantom(false, false);
    run_range_phantom(true, false);
}

fn run_absent_point_read(serializable: bool) {
    let (server, mut setup) = open_server_and_session();
    execute(
        &mut setup,
        "CREATE TABLE absent_rows (id INT PRIMARY KEY, data VARCHAR)",
    );

    let mut t1 = server.session();
    let mut t2 = server.session();
    begin_mode(&mut t1, serializable);
    begin_mode(&mut t2, serializable);
    assert!(query_rows(&mut t1, "SELECT * FROM absent_rows WHERE id = 10").is_empty());
    assert!(query_rows(&mut t2, "SELECT * FROM absent_rows WHERE id = 20").is_empty());
    execute(
        &mut t1,
        "INSERT INTO absent_rows (id, data) VALUES (20, 'a')",
    );
    execute(
        &mut t2,
        "INSERT INTO absent_rows (id, data) VALUES (10, 'b')",
    );
    execute(&mut t1, "COMMIT");
    if serializable {
        assert_serialization_failure(&execute_error(&mut t2, "COMMIT"));
    } else {
        execute(&mut t2, "COMMIT");
    }

    // Invariant: at most one of the two keys exists.
    let expected_count = if serializable { 1 } else { 2 };
    assert_eq!(
        query_rows(
            &mut setup,
            "SELECT COUNT(*) FROM absent_rows WHERE id IN (10, 20)"
        ),
        vec![Row::new(vec![Value::Int64(expected_count)])]
    );
}

#[test]
fn absent_point_read_write_skew_r5_path() {
    run_absent_point_read(false);
    run_absent_point_read(true);
}

#[test]
fn phantom_over_columnar_converted_partition() {
    run_range_phantom(false, true);
    run_range_phantom(true, true);
}

fn run_partitioned_analytic_skew(serializable: bool) {
    let (server, mut setup) = open_server_and_session();
    create_partitioned_table(&mut setup, "analytic_skew");
    execute(
        &mut setup,
        "INSERT INTO analytic_skew (id, value) VALUES (1, 10), (11, 20), (21, 30)",
    );

    let mut t1 = server.session();
    let mut t2 = server.session();
    begin_mode(&mut t1, serializable);
    begin_mode(&mut t2, serializable);
    assert_eq!(
        query_rows(&mut t1, "SELECT COUNT(*) FROM analytic_skew"),
        vec![Row::new(vec![Value::Int64(3)])]
    );
    assert_eq!(
        query_rows(&mut t2, "SELECT COUNT(*) FROM analytic_skew"),
        vec![Row::new(vec![Value::Int64(3)])]
    );
    execute(
        &mut t1,
        "INSERT INTO analytic_skew (id, value) VALUES (2, 40)",
    );
    execute(
        &mut t2,
        "INSERT INTO analytic_skew (id, value) VALUES (12, 50)",
    );
    execute(&mut t1, "COMMIT");
    if serializable {
        assert_serialization_failure(&execute_error(&mut t2, "COMMIT"));
    } else {
        execute(&mut t2, "COMMIT");
    }

    // Invariant: the table grows by at most one row from the count both observed.
    let expected_count = if serializable { 4 } else { 5 };
    assert_eq!(
        query_rows(&mut setup, "SELECT COUNT(*) FROM analytic_skew"),
        vec![Row::new(vec![Value::Int64(expected_count)])]
    );
}

#[test]
fn partitioned_table_analytic_scan_write_skew() {
    run_partitioned_analytic_skew(false);
    run_partitioned_analytic_skew(true);
}

fn run_correlated_subquery(serializable: bool) {
    let (server, mut setup) = open_server_and_session();
    execute(
        &mut setup,
        "CREATE TABLE tracked_reads (id INT PRIMARY KEY, val INT NOT NULL)",
    );
    execute(
        &mut setup,
        "INSERT INTO tracked_reads (id, val) VALUES (1, 10), (2, 20)",
    );

    let mut t1 = server.session();
    let mut t2 = server.session();
    begin_mode(&mut t1, serializable);
    begin_mode(&mut t2, serializable);
    assert_eq!(
        query_rows(
            &mut t1,
            "WITH eligible AS (SELECT id FROM tracked_reads WHERE val > 0) \
             SELECT * FROM tracked_reads WHERE id IN (SELECT id FROM eligible)"
        )
        .len(),
        2
    );
    assert_eq!(
        query_rows(
            &mut t2,
            "SELECT * FROM tracked_reads outer_reads WHERE EXISTS \
             (SELECT 1 FROM tracked_reads predicate_reads \
              WHERE predicate_reads.id = outer_reads.id AND predicate_reads.val > 0)"
        )
        .len(),
        2
    );
    execute(&mut t1, "UPDATE tracked_reads SET val = 0 WHERE id = 1");
    execute(&mut t2, "UPDATE tracked_reads SET val = 100 WHERE id = 2");
    execute(&mut t1, "COMMIT");

    // Under SI, both disjoint writes commit despite the crossed predicate reads. Under
    // SERIALIZABLE, the second commit must detect the read-write dependency cycle.
    if serializable {
        assert_serialization_failure(&execute_error(&mut t2, "COMMIT"));
    } else {
        execute(&mut t2, "COMMIT");
    }

    // Invariant: at most one of the two dependent updates may take effect.
    let expected_second_value = if serializable { 20 } else { 100 };
    assert_eq!(
        query_rows(&mut setup, "SELECT val FROM tracked_reads ORDER BY id"),
        vec![
            Row::new(vec![Value::Int32(0)]),
            Row::new(vec![Value::Int32(expected_second_value)]),
        ]
    );
}

#[test]
fn correlated_subquery_and_cte_reads_are_tracked() {
    run_correlated_subquery(false);
    run_correlated_subquery(true);
}

fn run_subquery_only_table_read(serializable: bool) {
    let (server, mut setup) = open_server_and_session();
    create_table(&mut setup, "subquery_outer_t");
    create_table(&mut setup, "subquery_inner_t");
    create_table(&mut setup, "subquery_writes");
    execute(
        &mut setup,
        "INSERT INTO subquery_outer_t (id, value) VALUES (1, 10)",
    );
    execute(
        &mut setup,
        "INSERT INTO subquery_inner_t (id, value) VALUES (1, 1)",
    );

    let mut reader_writer = server.session();
    let mut writer = server.session();
    begin_mode(&mut reader_writer, serializable);
    begin_mode(&mut writer, serializable);

    assert_eq!(
        query_rows(
            &mut reader_writer,
            "SELECT id FROM subquery_outer_t \
             WHERE id IN (SELECT id FROM subquery_inner_t WHERE value = 1)"
        ),
        vec![Row::new(vec![Value::Int32(1)])]
    );
    execute(
        &mut writer,
        "UPDATE subquery_inner_t SET value = 0 WHERE id = 1",
    );
    execute(&mut writer, "COMMIT");
    execute(
        &mut reader_writer,
        "INSERT INTO subquery_writes (id, value) VALUES (1, 10)",
    );

    if serializable {
        assert_serialization_failure(&execute_error(&mut reader_writer, "COMMIT"));
    } else {
        execute(&mut reader_writer, "COMMIT");
    }

    // Invariant: a transaction must not commit a dependent write from a stale subquery result.
    let expected_writes = if serializable { 0 } else { 1 };
    assert_eq!(
        query_rows(&mut setup, "SELECT COUNT(*) FROM subquery_writes"),
        vec![Row::new(vec![Value::Int64(expected_writes)])]
    );
}

#[test]
fn subquery_only_table_reads_are_tracked() {
    run_subquery_only_table_read(false);
    run_subquery_only_table_read(true);
}

fn run_cte_only_table_read(serializable: bool) {
    let (server, mut setup) = open_server_and_session();
    create_table(&mut setup, "cte_outer_t");
    create_table(&mut setup, "cte_inner_t");
    create_table(&mut setup, "cte_writes");
    execute(
        &mut setup,
        "INSERT INTO cte_outer_t (id, value) VALUES (1, 10)",
    );
    execute(
        &mut setup,
        "INSERT INTO cte_inner_t (id, value) VALUES (1, 1)",
    );

    let mut reader_writer = server.session();
    let mut writer = server.session();
    begin_mode(&mut reader_writer, serializable);
    begin_mode(&mut writer, serializable);

    assert_eq!(
        query_rows(
            &mut reader_writer,
            "WITH eligible AS (SELECT id FROM cte_inner_t WHERE value = 1) \
             SELECT id FROM cte_outer_t WHERE id IN (SELECT id FROM eligible)"
        ),
        vec![Row::new(vec![Value::Int32(1)])]
    );
    execute(&mut writer, "UPDATE cte_inner_t SET value = 0 WHERE id = 1");
    execute(&mut writer, "COMMIT");
    execute(
        &mut reader_writer,
        "INSERT INTO cte_writes (id, value) VALUES (1, 10)",
    );

    if serializable {
        assert_serialization_failure(&execute_error(&mut reader_writer, "COMMIT"));
    } else {
        execute(&mut reader_writer, "COMMIT");
    }

    // Invariant: a transaction must not commit a dependent write from a stale CTE result.
    let expected_writes = if serializable { 0 } else { 1 };
    assert_eq!(
        query_rows(&mut setup, "SELECT COUNT(*) FROM cte_writes"),
        vec![Row::new(vec![Value::Int64(expected_writes)])]
    );
}

#[test]
fn cte_only_table_reads_are_tracked() {
    run_cte_only_table_read(false);
    run_cte_only_table_read(true);
}

fn run_point_read_then_reorganize_cycle(serializable: bool) {
    let (server, mut setup) = open_server_and_session();
    create_partitioned_table(&mut setup, "point_reorganize_t");
    create_table(&mut setup, "point_reorganize_u");

    let mut t1 = server.session();
    let mut t2 = server.session();
    begin_mode(&mut t1, serializable);
    begin_mode(&mut t2, serializable);

    assert!(query_rows(&mut t1, "SELECT * FROM point_reorganize_t WHERE id = 15").is_empty());
    assert!(query_rows(&mut t2, "SELECT * FROM point_reorganize_u WHERE id = 1").is_empty());

    let mut ddl = server.session();
    execute(
        &mut ddl,
        "ALTER TABLE point_reorganize_t REORGANIZE PARTITION p1 INTO (\
             PARTITION p1a VALUES LESS THAN (15), \
             PARTITION p1b VALUES LESS THAN (20)\
         )",
    );

    execute(
        &mut t2,
        "INSERT INTO point_reorganize_t (id, value) VALUES (15, 20)",
    );
    execute(&mut t2, "COMMIT");

    execute(
        &mut t1,
        "INSERT INTO point_reorganize_u (id, value) VALUES (1, 10)",
    );
    if serializable {
        assert_serialization_failure(&execute_error(&mut t1, "COMMIT"));
    } else {
        execute(&mut t1, "COMMIT");
    }

    // Invariant: the crossed absent-key reads permit at most one corresponding insert.
    let expected_t_count = 1;
    let expected_u_count = if serializable { 0 } else { 1 };
    assert_eq!(
        query_rows(&mut setup, "SELECT COUNT(*) FROM point_reorganize_t"),
        vec![Row::new(vec![Value::Int64(expected_t_count)])]
    );
    assert_eq!(
        query_rows(&mut setup, "SELECT COUNT(*) FROM point_reorganize_u"),
        vec![Row::new(vec![Value::Int64(expected_u_count)])]
    );
}

#[test]
fn point_read_then_reorganize_cycle_is_prevented() {
    run_point_read_then_reorganize_cycle(false);
    run_point_read_then_reorganize_cycle(true);
}

fn run_update_by_key_then_reorganize(serializable: bool) {
    let (server, mut setup) = open_server_and_session();
    create_partitioned_table(&mut setup, "update_reorganize_t");
    create_table(&mut setup, "update_reorganize_u");

    let mut t1 = server.session();
    let mut t2 = server.session();
    begin_mode(&mut t1, serializable);
    begin_mode(&mut t2, serializable);

    execute(
        &mut t1,
        "UPDATE update_reorganize_t SET value = 11 WHERE id = 15",
    );
    assert!(query_rows(&mut t2, "SELECT * FROM update_reorganize_u WHERE id = 1").is_empty());

    let mut ddl = server.session();
    execute(
        &mut ddl,
        "ALTER TABLE update_reorganize_t REORGANIZE PARTITION p1 INTO (\
             PARTITION p1a VALUES LESS THAN (15), \
             PARTITION p1b VALUES LESS THAN (20)\
         )",
    );

    execute(
        &mut t2,
        "INSERT INTO update_reorganize_t (id, value) VALUES (15, 20)",
    );
    execute(&mut t2, "COMMIT");

    execute(
        &mut t1,
        "INSERT INTO update_reorganize_u (id, value) VALUES (1, 10)",
    );
    if serializable {
        assert_serialization_failure(&execute_error(&mut t1, "COMMIT"));
    } else {
        execute(&mut t1, "COMMIT");
    }

    // Invariant: the absent-key update and crossed read permit at most one corresponding insert.
    let expected_u_count = if serializable { 0 } else { 1 };
    assert_eq!(
        query_rows(&mut setup, "SELECT COUNT(*) FROM update_reorganize_t"),
        vec![Row::new(vec![Value::Int64(1)])]
    );
    assert_eq!(
        query_rows(&mut setup, "SELECT COUNT(*) FROM update_reorganize_u"),
        vec![Row::new(vec![Value::Int64(expected_u_count)])]
    );
}

#[test]
fn update_by_key_then_reorganize_is_prevented() {
    run_update_by_key_then_reorganize(false);
    run_update_by_key_then_reorganize(true);
}

#[test]
fn reread_after_reorganize_conflicts() {
    let (server, mut setup) = open_server_and_session();
    create_partitioned_table(&mut setup, "reread_reorganize_t");
    create_table(&mut setup, "reread_reorganize_u");
    execute(
        &mut setup,
        "INSERT INTO reread_reorganize_t (id, value) VALUES (1, 10)",
    );

    let mut t1 = server.session();
    begin_serializable(&mut t1);
    assert_eq!(
        query_rows(&mut t1, "SELECT COUNT(*) FROM reread_reorganize_t"),
        vec![Row::new(vec![Value::Int64(1)])]
    );

    let mut ddl = server.session();
    execute(
        &mut ddl,
        "ALTER TABLE reread_reorganize_t REORGANIZE PARTITION p1 INTO (\
             PARTITION p1a VALUES LESS THAN (15), \
             PARTITION p1b VALUES LESS THAN (20)\
         )",
    );

    assert!(query_rows(&mut t1, "SELECT * FROM reread_reorganize_t WHERE id < 10").len() == 1);
    execute(
        &mut t1,
        "INSERT INTO reread_reorganize_u (id, value) VALUES (1, 10)",
    );

    let error = execute_error(&mut t1, "COMMIT");
    assert!(
        error.to_ascii_lowercase().contains("catalog changed"),
        "unexpected catalog conflict: {error}"
    );
}

#[test]
fn disjoint_keys_both_commit() {
    let (server, mut setup) = open_server_and_session();
    create_table(&mut setup, "disjoint_keys");
    execute(
        &mut setup,
        "INSERT INTO disjoint_keys (id, value) VALUES (1, 10), (2, 20)",
    );

    let mut t1 = server.session();
    let mut t2 = server.session();
    begin_serializable(&mut t1);
    begin_serializable(&mut t2);
    execute(&mut t1, "SELECT * FROM disjoint_keys WHERE id = 1");
    execute(&mut t2, "SELECT * FROM disjoint_keys WHERE id = 2");
    execute(&mut t1, "UPDATE disjoint_keys SET value = 11 WHERE id = 1");
    execute(&mut t2, "UPDATE disjoint_keys SET value = 21 WHERE id = 2");
    execute(&mut t1, "COMMIT");
    execute(&mut t2, "COMMIT");
}

#[test]
fn declared_read_only_never_aborts_under_concurrent_writers() {
    let (server, mut setup) = open_server_and_session();
    create_table(&mut setup, "read_only_concurrent");
    execute(
        &mut setup,
        "INSERT INTO read_only_concurrent (id, value) VALUES (1, 10)",
    );

    let mut reader = server.session();
    let mut writer = server.session();
    execute(
        &mut reader,
        "START TRANSACTION ISOLATION LEVEL SERIALIZABLE READ ONLY",
    );
    begin_serializable(&mut writer);
    execute(&mut reader, "SELECT * FROM read_only_concurrent");
    execute(
        &mut writer,
        "UPDATE read_only_concurrent SET value = 20 WHERE id = 1",
    );
    execute(&mut writer, "COMMIT");
    execute(&mut reader, "COMMIT");
}

#[test]
fn reader_of_untouched_table_commits() {
    let (server, mut setup) = open_server_and_session();
    create_table(&mut setup, "untouched_a");
    create_table(&mut setup, "untouched_b");
    execute(
        &mut setup,
        "INSERT INTO untouched_a (id, value) VALUES (1, 10)",
    );
    execute(
        &mut setup,
        "INSERT INTO untouched_b (id, value) VALUES (1, 20)",
    );

    let mut t1 = server.session();
    let mut t2 = server.session();
    begin_serializable(&mut t1);
    begin_serializable(&mut t2);
    execute(&mut t1, "SELECT * FROM untouched_a");
    execute(&mut t2, "SELECT * FROM untouched_b");
    execute(&mut t1, "UPDATE untouched_a SET value = 11 WHERE id = 1");
    execute(&mut t2, "UPDATE untouched_b SET value = 21 WHERE id = 1");
    execute(&mut t1, "COMMIT");
    execute(&mut t2, "COMMIT");
}

#[test]
fn blind_writers_distinct_keys_commit() {
    let (server, mut setup) = open_server_and_session();
    create_table(&mut setup, "blind_writes");

    let mut t1 = server.session();
    let mut t2 = server.session();
    begin_serializable(&mut t1);
    begin_serializable(&mut t2);
    execute(
        &mut t1,
        "INSERT INTO blind_writes (id, value) VALUES (1, 10)",
    );
    execute(
        &mut t2,
        "INSERT INTO blind_writes (id, value) VALUES (2, 20)",
    );
    execute(&mut t1, "COMMIT");
    execute(&mut t2, "COMMIT");
}

#[test]
fn one_directional_rw_edge_aborts_reader_writer_documented_conservative() {
    let (server, mut setup) = open_server_and_session();
    create_table(&mut setup, "conservative_abort");
    execute(
        &mut setup,
        "INSERT INTO conservative_abort (id, value) VALUES (1, 10), (2, 20)",
    );

    let mut reader_writer = server.session();
    let mut blind_writer = server.session();
    begin_serializable(&mut reader_writer);
    begin_serializable(&mut blind_writer);
    execute(&mut reader_writer, "SELECT * FROM conservative_abort");
    execute(
        &mut reader_writer,
        "UPDATE conservative_abort SET value = 21 WHERE id = 2",
    );
    execute(
        &mut blind_writer,
        "INSERT INTO conservative_abort (id, value) VALUES (3, 30)",
    );
    execute(&mut blind_writer, "COMMIT");

    // Whole-partition footprints conservatively include a newly inserted row even though the
    // reader did not materialize that row. This intentionally permits a false-positive abort.
    assert_serialization_failure(&execute_error(&mut reader_writer, "COMMIT"));

    begin_serializable(&mut reader_writer);
    execute(&mut reader_writer, "SELECT * FROM conservative_abort");
    execute(
        &mut reader_writer,
        "UPDATE conservative_abort SET value = 22 WHERE id = 2",
    );
    execute(&mut reader_writer, "COMMIT");
}

#[test]
fn mixed_mode_si_partner_can_still_skew_documented() {
    let (server, mut setup) = open_server_and_session();
    create_table(&mut setup, "mixed_mode");
    execute(
        &mut setup,
        "INSERT INTO mixed_mode (id, value) VALUES (1, 1), (2, 1)",
    );

    let mut si = server.session();
    let mut serializable = server.session();
    begin_snapshot_isolation(&mut si);
    begin_serializable(&mut serializable);
    execute(&mut si, "SELECT * FROM mixed_mode");
    execute(&mut serializable, "SELECT * FROM mixed_mode");
    execute(
        &mut serializable,
        "UPDATE mixed_mode SET value = 0 WHERE id = 1",
    );
    execute(&mut serializable, "COMMIT");

    // Under SI, this doctors-style skew is possible. Phase 19 gives guarantees only to the
    // SERIALIZABLE participant, so after it commits the unvalidated SI participant can commit
    // its disjoint write and leave both rows off.
    execute(&mut si, "UPDATE mixed_mode SET value = 0 WHERE id = 2");
    execute(&mut si, "COMMIT");
    assert_eq!(
        query_rows(&mut setup, "SELECT value FROM mixed_mode ORDER BY id"),
        vec![
            Row::new(vec![Value::Int32(0)]),
            Row::new(vec![Value::Int32(0)]),
        ]
    );
}

#[test]
fn serializable_aborts_against_si_committed_writer() {
    let (server, mut setup) = open_server_and_session();
    create_table(&mut setup, "si_writer");
    execute(
        &mut setup,
        "INSERT INTO si_writer (id, value) VALUES (1, 10), (2, 20)",
    );

    let mut serializable = server.session();
    let mut si = server.session();
    begin_serializable(&mut serializable);
    begin_snapshot_isolation(&mut si);
    execute(&mut serializable, "SELECT * FROM si_writer WHERE id = 1");
    execute(&mut si, "UPDATE si_writer SET value = 11 WHERE id = 1");
    execute(&mut si, "COMMIT");
    execute(
        &mut serializable,
        "UPDATE si_writer SET value = 21 WHERE id = 2",
    );

    // Under SI, the two disjoint writes would both commit despite the stale read. The
    // SERIALIZABLE transaction instead aborts on the SI writer's read-write dependency.
    assert_serialization_failure(&execute_error(&mut serializable, "COMMIT"));

    // Control run: with both transactions using snapshot isolation, the same schedule commits.
    create_table(&mut setup, "si_control");
    execute(
        &mut setup,
        "INSERT INTO si_control (id, value) VALUES (1, 10), (2, 20)",
    );

    let mut stale_reader_writer = server.session();
    let mut concurrent_writer = server.session();
    begin_snapshot_isolation(&mut stale_reader_writer);
    begin_snapshot_isolation(&mut concurrent_writer);
    execute(
        &mut stale_reader_writer,
        "SELECT * FROM si_control WHERE id = 1",
    );
    execute(
        &mut concurrent_writer,
        "UPDATE si_control SET value = 11 WHERE id = 1",
    );
    execute(&mut concurrent_writer, "COMMIT");
    execute(
        &mut stale_reader_writer,
        "UPDATE si_control SET value = 21 WHERE id = 2",
    );
    execute(&mut stale_reader_writer, "COMMIT");

    assert_eq!(
        query_rows(&mut setup, "SELECT value FROM si_control ORDER BY id"),
        vec![
            Row::new(vec![Value::Int32(11)]),
            Row::new(vec![Value::Int32(21)]),
        ]
    );
}

#[test]
fn partition_added_after_read_conflicts_even_when_pruned() {
    let (server, mut setup) = open_server_and_session();
    execute(
        &mut setup,
        "CREATE TABLE partition_change (id INT PRIMARY KEY, value INT NOT NULL) \
         PARTITION BY RANGE (id) (\
             PARTITION p0 VALUES LESS THAN (10)\
         )",
    );
    execute(
        &mut setup,
        "INSERT INTO partition_change (id, value) VALUES (1, 10)",
    );

    let mut t1 = server.session();
    let mut ddl = server.session();
    begin_serializable(&mut t1);
    execute(&mut t1, "SELECT * FROM partition_change WHERE id < 10");

    execute(
        &mut ddl,
        "ALTER TABLE partition_change ADD PARTITION \
         (PARTITION p1 VALUES LESS THAN MAXVALUE)",
    );
    execute(
        &mut ddl,
        "INSERT INTO partition_change (id, value) VALUES (30, 30)",
    );

    execute(
        &mut t1,
        "UPDATE partition_change SET value = 11 WHERE id = 1",
    );
    let error = execute_error(&mut t1, "COMMIT");
    assert!(
        error.to_ascii_lowercase().contains("catalog changed"),
        "unexpected catalog conflict: {error}"
    );
}

#[test]
fn footprint_cap_promotes_to_partition() {
    let (_server, mut setup) = open_server_and_session();
    create_table(&mut setup, "footprint_cap");
    let values = (1..=120)
        .map(|id| format!("({id}, {id})"))
        .collect::<Vec<_>>()
        .join(", ");
    execute(
        &mut setup,
        &format!("INSERT INTO footprint_cap (id, value) VALUES {values}"),
    );

    begin_serializable(&mut setup);
    for id in 1..=101 {
        execute(
            &mut setup,
            &format!("SELECT * FROM footprint_cap WHERE id = {id}"),
        );
    }

    let htap_server::ReadFootprintSummary {
        point_key_counts: _,
        whole_partitions,
        table_partitions: _,
    } = setup
        .open_transaction_read_footprint()
        .expect("transaction footprint is available");
    assert!(
        !whole_partitions.is_empty(),
        "point reads beyond the cap must promote to a whole-partition footprint"
    );
    execute(&mut setup, "ROLLBACK");
}

fn run_import_commit_between_last_read_and_commit(serializable: bool) {
    let (server, mut setup) = open_server_and_session();
    create_table(&mut setup, "import_read_t");
    create_table(&mut setup, "import_write_t");

    let snapshot = htap_catalog::LocalCatalogStore::open(server.root.join("catalog"))
        .expect("catalog opens")
        .load()
        .expect("catalog loads")
        .expect("catalog snapshot exists");
    let table = snapshot
        .table_by_name("import_read_t")
        .expect("import table exists");
    let partition = snapshot
        .partition(table.partitions[0])
        .expect("import table partition exists");
    let options = htap_movement::CopyOptions::new(
        format!(
            "serializable-import-{}",
            if serializable { "ser" } else { "si" }
        ),
        table.id,
        partition.tablets[0],
        htap_movement::DataFormat::Csv,
        "import_read_t.csv",
    );

    let mut transaction = server.session();
    begin_mode(&mut transaction, serializable);
    assert!(query_rows(
        &mut transaction,
        "SELECT * FROM import_read_t WHERE id = 10"
    )
    .is_empty());
    execute(
        &mut transaction,
        "INSERT INTO import_write_t (id, value) VALUES (1, 10)",
    );

    server
        .server
        .copy_from_csv_reader(&options, std::io::Cursor::new("id,value\n10,20\n"))
        .expect("CSV import commits");

    if serializable {
        let error = execute_error(&mut transaction, "COMMIT");
        assert!(
            error.to_ascii_lowercase().contains("read-write dependency"),
            "unexpected serialization error: {error}"
        );
    } else {
        execute(&mut transaction, "COMMIT");
    }

    // Invariant: the dependent write must not coexist with an imported key absent from its read.
    let expected_writes = if serializable { 0 } else { 1 };
    assert_eq!(
        query_rows(&mut setup, "SELECT COUNT(*) FROM import_read_t"),
        vec![Row::new(vec![Value::Int64(1)])]
    );
    assert_eq!(
        query_rows(&mut setup, "SELECT COUNT(*) FROM import_write_t"),
        vec![Row::new(vec![Value::Int64(expected_writes)])]
    );
}

#[test]
fn import_commit_between_last_read_and_commit_conflicts() {
    run_import_commit_between_last_read_and_commit(false);
    run_import_commit_between_last_read_and_commit(true);
}
