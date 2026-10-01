use std::collections::{BTreeMap, BTreeSet};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use htap_common::types::{Row, Value};
#[cfg(unix)]
use htap_server::ipc::{
    read_frame, write_frame, IpcRequest, IpcResponse, ResponsePayload, IPC_PROTOCOL_VERSION,
};
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
            "htap-server-serializable-session-{}-{timestamp}-{unique_id}",
            std::process::id()
        ));
        let server = Arc::new(LocalServer::open(&root).expect("server opens"));
        Self { root, server }
    }

    fn session(&self) -> Session {
        self.server.open_session().expect("session opens")
    }

    fn serializable_pinned_count(&self) -> usize {
        self.server
            .txn_manager()
            .expect("owner server exposes transaction manager")
            .serializable_pinned_count()
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

fn assert_isolation(session: &mut Session, expected: &str) {
    assert_eq!(
        query_rows(session, "SELECT @@transaction_isolation"),
        vec![Row::new(vec![Value::String(expected.to_string())])]
    );
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

type ReadFootprint = (BTreeMap<u64, usize>, BTreeSet<u64>, BTreeMap<u64, Vec<u64>>);

fn footprint(session: &Session) -> ReadFootprint {
    let htap_server::ReadFootprintSummary {
        point_key_counts,
        whole_partitions,
        table_partitions,
    } = session
        .open_transaction_read_footprint()
        .expect("transaction footprint is available");

    (point_key_counts, whole_partitions, table_partitions)
}

fn begin_serializable(session: &mut Session) {
    execute(session, "START TRANSACTION ISOLATION LEVEL SERIALIZABLE");
}

fn begin_snapshot_isolation(session: &mut Session) {
    execute(session, "START TRANSACTION ISOLATION LEVEL REPEATABLE READ");
}

#[cfg(unix)]
fn request(stream: &mut UnixStream, request: IpcRequest) -> IpcResponse {
    write_frame(stream, &request).unwrap();
    read_frame(stream).unwrap()
}

#[test]
fn point_select_by_pk_records_key_not_partition() {
    let (_server, mut session) = open_server_and_session();
    create_table(&mut session, "points");
    execute(
        &mut session,
        "INSERT INTO points (id, value) VALUES (1, 10)",
    );

    begin_serializable(&mut session);
    execute(&mut session, "SELECT * FROM points WHERE id = 1");

    let (point_key_counts, whole_partitions, table_partitions) = footprint(&session);
    assert_eq!(point_key_counts.values().sum::<usize>(), 1);
    assert_eq!(point_key_counts.len(), 1);
    assert!(whole_partitions.is_empty());
    assert_eq!(table_partitions.len(), 1);
    assert_eq!(
        table_partitions
            .values()
            .next()
            .expect("table partition set is recorded")
            .len(),
        1
    );

    execute(&mut session, "ROLLBACK");
}

#[test]
fn scan_records_partition_even_when_served_from_own_writes() {
    let (_server, mut session) = open_server_and_session();
    create_table(&mut session, "own_writes");

    begin_serializable(&mut session);
    execute(
        &mut session,
        "INSERT INTO own_writes (id, value) VALUES (1, 10)",
    );
    execute(&mut session, "SELECT * FROM own_writes");

    let (point_key_counts, whole_partitions, table_partitions) = footprint(&session);
    assert!(point_key_counts.is_empty());
    assert_eq!(whole_partitions.len(), 1);
    assert_eq!(table_partitions.len(), 1);
    assert_eq!(table_partitions.values().map(Vec::len).sum::<usize>(), 1);

    execute(&mut session, "ROLLBACK");
}

#[test]
fn analytic_worker_scan_records_every_partition() {
    let (_server, mut session) = open_server_and_session();
    create_partitioned_table(&mut session, "analytic_parts");
    execute(
        &mut session,
        "INSERT INTO analytic_parts (id, value) VALUES (1, 10), (11, 20), (21, 30)",
    );

    begin_serializable(&mut session);
    execute(&mut session, "SELECT COUNT(*) FROM analytic_parts");

    let (point_key_counts, whole_partitions, table_partitions) = footprint(&session);
    assert!(point_key_counts.is_empty());
    assert_eq!(whole_partitions.len(), 3);
    assert_eq!(table_partitions.len(), 1);
    assert_eq!(
        table_partitions
            .values()
            .next()
            .expect("table partition set is recorded")
            .len(),
        3
    );

    execute(&mut session, "ROLLBACK");
}

#[test]
fn pruned_scan_records_table_partition_set() {
    let (_server, mut session) = open_server_and_session();
    create_partitioned_table(&mut session, "pruned_parts");
    execute(
        &mut session,
        "INSERT INTO pruned_parts (id, value) VALUES (1, 10), (11, 20), (21, 30)",
    );

    begin_serializable(&mut session);
    execute(
        &mut session,
        "SELECT COUNT(*) FROM pruned_parts WHERE id < 10",
    );

    let (_point_key_counts, whole_partitions, table_partitions) = footprint(&session);
    assert_eq!(whole_partitions.len(), 1);
    assert_eq!(table_partitions.len(), 1);
    assert_eq!(
        table_partitions
            .values()
            .next()
            .expect("table partition set is recorded")
            .len(),
        3,
        "the logical table dependency must retain the pre-pruning partition set"
    );

    execute(&mut session, "ROLLBACK");
}

#[test]
fn update_where_delete_where_insert_select_record_reads() {
    let (_server, mut session) = open_server_and_session();
    create_table(&mut session, "source_rows");
    create_table(&mut session, "copied_rows");
    execute(
        &mut session,
        "INSERT INTO source_rows (id, value) VALUES (1, 10), (2, 20), (3, 30)",
    );

    begin_serializable(&mut session);
    execute(
        &mut session,
        "UPDATE source_rows SET value = value + 1 WHERE value >= 20",
    );
    let (_, update_partitions, update_tables) = footprint(&session);
    assert_eq!(update_partitions.len(), 1);
    assert_eq!(update_tables.len(), 1);
    execute(&mut session, "ROLLBACK");

    begin_serializable(&mut session);
    execute(&mut session, "DELETE FROM source_rows WHERE value >= 20");
    let (_, delete_partitions, delete_tables) = footprint(&session);
    assert_eq!(delete_partitions.len(), 1);
    assert_eq!(delete_tables.len(), 1);
    execute(&mut session, "ROLLBACK");

    begin_serializable(&mut session);
    execute(
        &mut session,
        "INSERT INTO copied_rows (id, value) SELECT id, value FROM source_rows WHERE value >= 20",
    );
    let (_, insert_select_partitions, insert_select_tables) = footprint(&session);
    assert_eq!(insert_select_partitions.len(), 1);
    assert_eq!(insert_select_tables.len(), 1);
    execute(&mut session, "ROLLBACK");
}

#[test]
fn write_skew_is_prevented_under_serializable_and_allowed_under_si() {
    let (server, mut setup) = open_server_and_session();
    create_table(&mut setup, "si_skew");
    create_table(&mut setup, "serializable_skew");
    execute(
        &mut setup,
        "INSERT INTO si_skew (id, value) VALUES (1, 1), (2, 1)",
    );
    execute(
        &mut setup,
        "INSERT INTO serializable_skew (id, value) VALUES (1, 1), (2, 1)",
    );

    let mut si_left = server.session();
    let mut si_right = server.session();
    begin_snapshot_isolation(&mut si_left);
    begin_snapshot_isolation(&mut si_right);
    execute(&mut si_left, "SELECT * FROM si_skew");
    execute(&mut si_right, "SELECT * FROM si_skew");
    execute(&mut si_left, "UPDATE si_skew SET value = 0 WHERE id = 1");
    execute(&mut si_right, "UPDATE si_skew SET value = 0 WHERE id = 2");
    execute(&mut si_left, "COMMIT");
    execute(&mut si_right, "COMMIT");

    let mut serializable_left = server.session();
    let mut serializable_right = server.session();
    begin_serializable(&mut serializable_left);
    begin_serializable(&mut serializable_right);
    execute(&mut serializable_left, "SELECT * FROM serializable_skew");
    execute(&mut serializable_right, "SELECT * FROM serializable_skew");
    execute(
        &mut serializable_left,
        "UPDATE serializable_skew SET value = 0 WHERE id = 1",
    );
    execute(
        &mut serializable_right,
        "UPDATE serializable_skew SET value = 0 WHERE id = 2",
    );
    execute(&mut serializable_left, "COMMIT");

    let error = execute_error(&mut serializable_right, "COMMIT");
    assert!(
        error.contains("read-write dependency"),
        "unexpected serialization error: {error}"
    );
}

#[test]
fn declared_read_only_serializable_never_aborts() {
    let (server, mut setup) = open_server_and_session();
    create_table(&mut setup, "read_only_rows");
    execute(
        &mut setup,
        "INSERT INTO read_only_rows (id, value) VALUES (1, 10)",
    );

    let mut reader = server.session();
    let mut writer = server.session();

    execute(
        &mut reader,
        "START TRANSACTION ISOLATION LEVEL SERIALIZABLE READ ONLY",
    );
    execute(&mut reader, "SELECT * FROM read_only_rows");

    execute(
        &mut writer,
        "UPDATE read_only_rows SET value = 20 WHERE id = 1",
    );

    execute(&mut reader, "COMMIT");
    assert!(!reader.in_transaction());
}

#[test]
fn empty_write_set_serializable_commit_skips_validation() {
    let (server, mut setup) = open_server_and_session();
    create_table(&mut setup, "read_without_write");
    execute(
        &mut setup,
        "INSERT INTO read_without_write (id, value) VALUES (1, 10)",
    );

    let mut reader = server.session();
    let mut writer = server.session();

    begin_serializable(&mut reader);
    execute(&mut reader, "SELECT * FROM read_without_write");

    execute(
        &mut writer,
        "UPDATE read_without_write SET value = 20 WHERE id = 1",
    );

    execute(&mut reader, "COMMIT");
    assert!(!reader.in_transaction());
}

#[test]
fn read_only_table_dropped_before_commit_conflicts() {
    let (server, mut setup) = open_server_and_session();
    create_table(&mut setup, "serializable_read_source");
    create_table(&mut setup, "serializable_write_target");
    execute(
        &mut setup,
        "INSERT INTO serializable_read_source (id, value) VALUES (1, 10)",
    );

    let mut transaction = server.session();
    let mut ddl = server.session();

    begin_serializable(&mut transaction);
    execute(&mut transaction, "SELECT * FROM serializable_read_source");
    execute(
        &mut transaction,
        "INSERT INTO serializable_write_target (id, value) VALUES (1, 20)",
    );

    execute(&mut ddl, "DROP TABLE serializable_read_source");

    let error = execute_error(&mut transaction, "COMMIT");
    assert!(
        error.contains("catalog changed"),
        "unexpected catalog conflict: {error}"
    );
    assert!(!transaction.in_transaction());
}

#[test]
fn drop_table_between_read_and_commit_conflicts() {
    let (server, mut setup) = open_server_and_session();
    create_table(&mut setup, "dropped_after_read");
    execute(
        &mut setup,
        "INSERT INTO dropped_after_read (id, value) VALUES (1, 10)",
    );

    let mut reader = server.session();
    let mut ddl = server.session();

    begin_serializable(&mut reader);
    execute(&mut reader, "SELECT * FROM dropped_after_read");
    execute(
        &mut reader,
        "UPDATE dropped_after_read SET value = 11 WHERE id = 1",
    );

    execute(&mut ddl, "DROP TABLE dropped_after_read");

    let error = execute_error(&mut reader, "COMMIT");
    assert!(
        error.contains("table dropped or partition altered during transaction"),
        "unexpected catalog conflict: {error}"
    );
    assert!(!reader.in_transaction());
}

#[test]
fn ticket_released_on_commit_rollback_and_session_drop() {
    let (server, mut setup) = open_server_and_session();
    create_table(&mut setup, "ticket_rows");

    {
        let mut session = server.session();
        begin_serializable(&mut session);
        assert_eq!(server.serializable_pinned_count(), 1);
        execute(&mut session, "COMMIT");
        assert_eq!(server.serializable_pinned_count(), 0);
    }

    {
        let mut session = server.session();
        begin_serializable(&mut session);
        assert_eq!(server.serializable_pinned_count(), 1);
        execute(&mut session, "ROLLBACK");
        assert_eq!(server.serializable_pinned_count(), 0);
    }

    {
        let mut session = server.session();
        begin_serializable(&mut session);
        assert_eq!(server.serializable_pinned_count(), 1);
    }
    assert_eq!(server.serializable_pinned_count(), 0);
}

#[test]
fn one_shot_isolation_is_consumed_by_select_like_mysql() {
    let (_server, mut session) = open_server_and_session();

    execute(&mut session, "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE");
    assert_isolation(&mut session, "SERIALIZABLE");
    assert_isolation(&mut session, "REPEATABLE-READ");
}

#[test]
fn unscoped_transaction_isolation_assignment_is_one_shot() {
    let (_server, mut session) = open_server_and_session();

    execute(
        &mut session,
        "SET SESSION TRANSACTION ISOLATION LEVEL SERIALIZABLE",
    );
    execute(
        &mut session,
        "SET @@transaction_isolation = 'REPEATABLE-READ'",
    );

    execute(&mut session, "START TRANSACTION");
    assert_isolation(&mut session, "REPEATABLE-READ");
    execute(&mut session, "COMMIT");

    execute(&mut session, "START TRANSACTION");
    assert_isolation(&mut session, "SERIALIZABLE");
    execute(&mut session, "COMMIT");
}

#[test]
fn one_shot_isolation_is_consumed_by_autocommit_write() {
    let (server, mut session) = open_server_and_session();
    create_table(&mut session, "autocommit_serializable");
    execute(
        &mut session,
        "INSERT INTO autocommit_serializable (id, value) VALUES (1, 10), (2, 20)",
    );

    assert_isolation(&mut session, "REPEATABLE-READ");
    execute(&mut session, "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE");
    execute(
        &mut session,
        "UPDATE autocommit_serializable SET value = value + 1 WHERE id = 1",
    );
    assert_isolation(&mut session, "REPEATABLE-READ");

    assert_eq!(
        query_rows(
            &mut session,
            "SELECT id, value FROM autocommit_serializable WHERE id = 1",
        ),
        vec![Row::new(vec![Value::Int32(1), Value::Int32(11)])]
    );
    assert!(!session.in_transaction());
    assert_eq!(server.serializable_pinned_count(), 0);
}

#[test]
fn recovery_required_keeps_serializable_txn_retryable() {
    let (server, mut setup) = open_server_and_session();
    create_table(&mut setup, "recovery_retry");
    execute(
        &mut setup,
        "INSERT INTO recovery_retry (id, value) VALUES (1, 10), (2, 20)",
    );

    let mut blocker = server.session();
    begin_snapshot_isolation(&mut blocker);
    execute(
        &mut blocker,
        "UPDATE recovery_retry SET value = 11 WHERE id = 1",
    );
    server
        .server
        .txn_manager()
        .expect("owner server exposes transaction manager")
        .set_commit_append_hook(|_journal| {
            Err(htap_common::HtapError::Io(std::io::Error::other(
                "simulated commit append failure",
            )))
        });
    let blocker_error = execute_error(&mut blocker, "COMMIT");
    assert!(
        blocker_error.to_ascii_lowercase().contains("recovery"),
        "unexpected blocker error: {blocker_error}"
    );

    let mut session = server.session();
    begin_serializable(&mut session);
    execute(&mut session, "SELECT * FROM recovery_retry WHERE id = 2");
    execute(
        &mut session,
        "UPDATE recovery_retry SET value = 21 WHERE id = 2",
    );

    let error = execute_error(&mut session, "COMMIT");
    assert!(
        error.to_ascii_lowercase().contains("recovery"),
        "unexpected recovery-required error: {error}"
    );
    assert!(session.in_transaction());
    assert_eq!(server.serializable_pinned_count(), 1);
    execute(&mut session, "SELECT * FROM recovery_retry WHERE id = 2");
    server
        .server
        .txn_manager()
        .expect("owner server exposes transaction manager")
        .set_commit_append_hook(|_journal| Ok(()));
    execute(&mut session, "ROLLBACK");
    assert_eq!(server.serializable_pinned_count(), 0);
}

#[test]
fn recovery_required_keeps_read_footprint() {
    let (server, mut setup) = open_server_and_session();
    create_table(&mut setup, "recovery_footprint");
    execute(
        &mut setup,
        "INSERT INTO recovery_footprint (id, value) VALUES (1, 10), (2, 20)",
    );

    let mut blocker = server.session();
    begin_snapshot_isolation(&mut blocker);
    execute(
        &mut blocker,
        "UPDATE recovery_footprint SET value = 11 WHERE id = 1",
    );
    server
        .server
        .txn_manager()
        .expect("owner server exposes transaction manager")
        .set_commit_append_hook(|_journal| {
            Err(htap_common::HtapError::Io(std::io::Error::other(
                "simulated commit append failure",
            )))
        });
    let blocker_error = execute_error(&mut blocker, "COMMIT");
    assert!(
        blocker_error.to_ascii_lowercase().contains("recovery"),
        "unexpected blocker error: {blocker_error}"
    );

    let mut session = server.session();
    begin_serializable(&mut session);
    execute(
        &mut session,
        "SELECT * FROM recovery_footprint WHERE id = 2",
    );
    execute(
        &mut session,
        "UPDATE recovery_footprint SET value = 21 WHERE id = 2",
    );
    let footprint_before_commit = footprint(&session);

    let error = execute_error(&mut session, "COMMIT");
    assert!(
        error.to_ascii_lowercase().contains("recovery"),
        "unexpected recovery-required error: {error}"
    );
    assert!(session.in_transaction());
    assert_eq!(footprint(&session), footprint_before_commit);

    server
        .server
        .txn_manager()
        .expect("owner server exposes transaction manager")
        .set_commit_append_hook(|_journal| Ok(()));
    execute(&mut session, "ROLLBACK");
}

#[test]
fn set_session_and_local_transaction_isolation_persist() {
    let (_server, mut session) = open_server_and_session();

    execute(
        &mut session,
        "SET SESSION TRANSACTION ISOLATION LEVEL SERIALIZABLE",
    );
    assert_isolation(&mut session, "SERIALIZABLE");

    for _ in 0..2 {
        execute(&mut session, "START TRANSACTION");
        assert_isolation(&mut session, "SERIALIZABLE");
        execute(&mut session, "COMMIT");
    }

    execute(
        &mut session,
        "SET LOCAL TRANSACTION ISOLATION LEVEL REPEATABLE READ",
    );
    assert_isolation(&mut session, "REPEATABLE-READ");

    for _ in 0..2 {
        execute(&mut session, "START TRANSACTION");
        assert_isolation(&mut session, "REPEATABLE-READ");
        execute(&mut session, "COMMIT");
    }
}

#[test]
fn set_global_transaction_isolation_is_rejected() {
    let (_server, mut session) = open_server_and_session();

    assert_isolation(&mut session, "REPEATABLE-READ");
    let error = htap_sql::parse_one("SET GLOBAL TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .expect_err("SET GLOBAL TRANSACTION must be rejected at parse time")
        .to_string();
    assert!(
        error.to_ascii_lowercase().contains("global transaction"),
        "unexpected global isolation error: {error}"
    );
    assert_isolation(&mut session, "REPEATABLE-READ");
}

#[test]
fn one_shot_isolation_is_consumed_by_explicit_start() {
    let (_server, mut session) = open_server_and_session();

    execute(
        &mut session,
        "SET SESSION TRANSACTION ISOLATION LEVEL SERIALIZABLE",
    );
    execute(
        &mut session,
        "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
    );

    execute(
        &mut session,
        "START TRANSACTION ISOLATION LEVEL SERIALIZABLE",
    );
    assert_isolation(&mut session, "SERIALIZABLE");
    execute(&mut session, "COMMIT");

    execute(&mut session, "START TRANSACTION");
    assert_isolation(&mut session, "SERIALIZABLE");
    execute(&mut session, "COMMIT");
}

#[test]
fn explain_analyze_update_in_serializable_autocommit_session() {
    let (server, mut session) = open_server_and_session();
    create_table(&mut session, "explain_update");
    execute(
        &mut session,
        "INSERT INTO explain_update (id, value) VALUES (1, 10)",
    );

    execute(
        &mut session,
        "SET SESSION transaction_isolation = 'SERIALIZABLE'",
    );
    execute(
        &mut session,
        "EXPLAIN ANALYZE UPDATE explain_update SET value = value + 1 WHERE id = 1",
    );

    assert_eq!(
        query_rows(
            &mut session,
            "SELECT id, value FROM explain_update WHERE id = 1",
        ),
        vec![Row::new(vec![Value::Int32(1), Value::Int32(11)])]
    );
    assert!(!session.in_transaction());
    assert_eq!(server.serializable_pinned_count(), 0);
}

#[test]
fn ticket_released_after_validation_conflict_and_implicit_commit() {
    let (server, mut setup) = open_server_and_session();
    create_table(&mut setup, "ticket_skew");
    create_table(&mut setup, "ticket_implicit");
    execute(
        &mut setup,
        "INSERT INTO ticket_skew (id, value) VALUES (1, 1), (2, 1)",
    );
    execute(
        &mut setup,
        "INSERT INTO ticket_implicit (id, value) VALUES (1, 10)",
    );

    let mut left = server.session();
    let mut right = server.session();
    begin_serializable(&mut left);
    begin_serializable(&mut right);
    execute(&mut left, "SELECT * FROM ticket_skew");
    execute(&mut right, "SELECT * FROM ticket_skew");
    execute(&mut left, "UPDATE ticket_skew SET value = 0 WHERE id = 1");
    execute(&mut right, "UPDATE ticket_skew SET value = 0 WHERE id = 2");
    execute(&mut left, "COMMIT");

    let error = execute_error(&mut right, "COMMIT");
    assert!(
        error.contains("read-write dependency"),
        "unexpected serialization error: {error}"
    );
    assert!(!right.in_transaction());
    assert_eq!(server.serializable_pinned_count(), 0);

    let mut implicit = server.session();
    execute(
        &mut implicit,
        "SET SESSION transaction_isolation = 'SERIALIZABLE'",
    );
    execute(&mut implicit, "SET autocommit = 0");
    execute(
        &mut implicit,
        "UPDATE ticket_implicit SET value = value + 1 WHERE id = 1",
    );
    assert!(implicit.in_transaction());
    assert_eq!(server.serializable_pinned_count(), 1);

    execute(&mut implicit, "START TRANSACTION");
    assert!(implicit.in_transaction());
    assert_eq!(server.serializable_pinned_count(), 1);

    execute(&mut implicit, "COMMIT");
    assert!(!implicit.in_transaction());
    assert_eq!(server.serializable_pinned_count(), 0);
}

#[test]
fn set_forms_select_isolation_level_and_persist() {
    let (_server, mut session) = open_server_and_session();

    execute(
        &mut session,
        "SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL SERIALIZABLE",
    );
    assert_isolation(&mut session, "SERIALIZABLE");

    execute(&mut session, "START TRANSACTION");
    assert_isolation(&mut session, "SERIALIZABLE");
    execute(&mut session, "COMMIT");

    execute(
        &mut session,
        "SET SESSION transaction_isolation = 'REPEATABLE READ'",
    );
    assert_isolation(&mut session, "REPEATABLE-READ");

    execute(&mut session, "START TRANSACTION");
    assert_isolation(&mut session, "REPEATABLE-READ");
    execute(&mut session, "COMMIT");
}

#[test]
fn read_committed_and_read_uncommitted_still_unsupported() {
    let (_server, mut session) = open_server_and_session();

    for isolation in ["READ COMMITTED", "READ UNCOMMITTED"] {
        let error = execute_error(
            &mut session,
            &format!("SET SESSION transaction_isolation = '{isolation}'"),
        );
        assert!(
            error.to_ascii_lowercase().contains("unsupported"),
            "unexpected error for {isolation}: {error}"
        );

        let error = execute_error(
            &mut session,
            &format!("START TRANSACTION ISOLATION LEVEL {isolation}"),
        );
        assert!(
            error.to_ascii_lowercase().contains("unsupported"),
            "unexpected error for {isolation}: {error}"
        );
    }
}

#[test]
fn set_transaction_inside_open_txn_applies_to_next_txn() {
    let (_server, mut session) = open_server_and_session();

    begin_snapshot_isolation(&mut session);
    assert_isolation(&mut session, "REPEATABLE-READ");

    execute(&mut session, "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE");
    assert_isolation(&mut session, "REPEATABLE-READ");
    execute(&mut session, "COMMIT");

    execute(&mut session, "START TRANSACTION");
    assert_isolation(&mut session, "SERIALIZABLE");
    execute(&mut session, "COMMIT");

    execute(&mut session, "START TRANSACTION");
    assert_isolation(&mut session, "REPEATABLE-READ");
    execute(&mut session, "COMMIT");
}

#[test]
fn global_isolation_is_rejected() {
    let (_server, mut session) = open_server_and_session();

    let error = execute_error(
        &mut session,
        "SET GLOBAL transaction_isolation = 'SERIALIZABLE'",
    );
    assert!(
        error.to_ascii_lowercase().contains("global"),
        "unexpected global isolation error: {error}"
    );
}

#[test]
fn serializable_over_ipc_owner_session() {
    let root = tempfile::tempdir().expect("temporary root");
    let server = Arc::new(LocalServer::open(root.path()).expect("server opens"));
    for table in ["serializable_doctors", "si_doctors"] {
        server
            .execute(&format!(
                "CREATE TABLE {table} (id INT NOT NULL PRIMARY KEY, on_call BOOLEAN NOT NULL)"
            ))
            .expect("table creates");
        server
            .execute(&format!(
                "INSERT INTO {table} (id, on_call) VALUES (1, TRUE), (2, TRUE)"
            ))
            .expect("seed rows insert");
    }

    let socket_path = root.path().join("htap.sock");
    let mut left_stream = UnixStream::connect(&socket_path).expect("left connects");
    left_stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("left read timeout sets");
    let mut right_stream = UnixStream::connect(&socket_path).expect("right connects");
    right_stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("right read timeout sets");

    let canonical_root = root.path().canonicalize().expect("root canonicalizes");
    for stream in [&mut left_stream, &mut right_stream] {
        let handshake = request(
            stream,
            IpcRequest::Handshake {
                protocol_version: IPC_PROTOCOL_VERSION,
                canonical_root: canonical_root.to_string_lossy().into_owned(),
            },
        );
        assert!(
            matches!(handshake.result, Ok(ResponsePayload::Empty)),
            "handshake failed: {handshake:?}"
        );
    }

    let left_open = request(&mut left_stream, IpcRequest::OpenSession);
    let left_session_id = match left_open.result {
        Ok(ResponsePayload::SessionId(session_id)) => session_id,
        _ => panic!("left session failed to open: {left_open:?}"),
    };
    let right_open = request(&mut right_stream, IpcRequest::OpenSession);
    let right_session_id = match right_open.result {
        Ok(ResponsePayload::SessionId(session_id)) => session_id,
        _ => panic!("right session failed to open: {right_open:?}"),
    };

    for (stream, isolation) in [
        (&mut left_stream, "SERIALIZABLE"),
        (&mut right_stream, "SERIALIZABLE"),
    ] {
        let set_isolation = request(
            stream,
            IpcRequest::Execute {
                sql: format!("SET SESSION transaction_isolation = '{isolation}'"),
            },
        );
        assert!(
            matches!(
                set_isolation.result,
                Ok(ResponsePayload::StatementResult(_))
            ),
            "setting {isolation} isolation failed: {set_isolation:?}"
        );
    }

    for (stream, session_id) in [
        (&mut left_stream, left_session_id),
        (&mut right_stream, right_session_id),
    ] {
        let begin = request(stream, IpcRequest::Begin { session_id });
        assert!(
            matches!(begin.result, Ok(ResponsePayload::Empty)),
            "serializable begin failed: {begin:?}"
        );
        assert!(begin.status.in_transaction);

        let read = request(
            stream,
            IpcRequest::Execute {
                sql: "SELECT id, on_call FROM serializable_doctors WHERE id IN (1, 2)".into(),
            },
        );
        assert!(
            matches!(read.result, Ok(ResponsePayload::StatementResult(_))),
            "serializable read failed: {read:?}"
        );
    }

    let left_write = request(
        &mut left_stream,
        IpcRequest::Execute {
            sql: "UPDATE serializable_doctors SET on_call = FALSE WHERE id = 1".into(),
        },
    );
    assert!(
        matches!(left_write.result, Ok(ResponsePayload::StatementResult(_))),
        "left serializable write failed: {left_write:?}"
    );

    let right_write = request(
        &mut right_stream,
        IpcRequest::Execute {
            sql: "UPDATE serializable_doctors SET on_call = FALSE WHERE id = 2".into(),
        },
    );
    assert!(
        matches!(right_write.result, Ok(ResponsePayload::StatementResult(_))),
        "right serializable write failed: {right_write:?}"
    );

    let left_commit = request(
        &mut left_stream,
        IpcRequest::Commit {
            session_id: left_session_id,
        },
    );
    assert!(
        matches!(left_commit.result, Ok(ResponsePayload::Empty)),
        "left serializable commit failed: {left_commit:?}"
    );

    let right_commit = request(
        &mut right_stream,
        IpcRequest::Commit {
            session_id: right_session_id,
        },
    );
    let right_error = right_commit
        .result
        .as_ref()
        .expect_err("crossed serializable write skew must abort");
    let htap_server::ipc::WireError::Conflict(message) = right_error else {
        panic!("unexpected serialization error: {right_error:?}");
    };
    assert!(
        message.contains("read-write dependency"),
        "unexpected serialization error: {message}"
    );
    assert!(
        !right_commit.status.in_transaction,
        "failed commit left the right session in a transaction: {right_commit:?}"
    );

    let result = server
        .execute("SELECT id, on_call FROM serializable_doctors ORDER BY id")
        .expect("serializable doctors read");
    let StatementResult::Query(query) = result else {
        panic!("expected query result, got {result:?}");
    };
    assert_eq!(
        query.rows,
        vec![
            Row::new(vec![Value::Int32(1), Value::Bool(false)]),
            Row::new(vec![Value::Int32(2), Value::Bool(true)]),
        ]
    );

    for (stream, isolation) in [
        (&mut left_stream, "REPEATABLE READ"),
        (&mut right_stream, "REPEATABLE READ"),
    ] {
        let set_isolation = request(
            stream,
            IpcRequest::Execute {
                sql: format!("SET SESSION transaction_isolation = '{isolation}'"),
            },
        );
        assert!(
            matches!(
                set_isolation.result,
                Ok(ResponsePayload::StatementResult(_))
            ),
            "setting {isolation} isolation failed: {set_isolation:?}"
        );
    }

    for (stream, session_id) in [
        (&mut left_stream, left_session_id),
        (&mut right_stream, right_session_id),
    ] {
        let begin = request(stream, IpcRequest::Begin { session_id });
        assert!(
            matches!(begin.result, Ok(ResponsePayload::Empty)),
            "repeatable-read begin failed: {begin:?}"
        );

        let read = request(
            stream,
            IpcRequest::Execute {
                sql: "SELECT id, on_call FROM si_doctors WHERE id IN (1, 2)".into(),
            },
        );
        assert!(
            matches!(read.result, Ok(ResponsePayload::StatementResult(_))),
            "repeatable-read read failed: {read:?}"
        );
    }

    for (stream, sql) in [
        (
            &mut left_stream,
            "UPDATE si_doctors SET on_call = FALSE WHERE id = 1",
        ),
        (
            &mut right_stream,
            "UPDATE si_doctors SET on_call = FALSE WHERE id = 2",
        ),
    ] {
        let write = request(stream, IpcRequest::Execute { sql: sql.into() });
        assert!(
            matches!(write.result, Ok(ResponsePayload::StatementResult(_))),
            "repeatable-read write failed: {write:?}"
        );
    }

    for (stream, session_id) in [
        (&mut left_stream, left_session_id),
        (&mut right_stream, right_session_id),
    ] {
        let commit = request(stream, IpcRequest::Commit { session_id });
        assert!(
            matches!(commit.result, Ok(ResponsePayload::Empty)),
            "repeatable-read commit failed: {commit:?}"
        );
    }

    let result = server
        .execute("SELECT id, on_call FROM si_doctors ORDER BY id")
        .expect("repeatable-read doctors read");
    let StatementResult::Query(query) = result else {
        panic!("expected query result, got {result:?}");
    };
    assert_eq!(
        query.rows,
        vec![
            Row::new(vec![Value::Int32(1), Value::Bool(false)]),
            Row::new(vec![Value::Int32(2), Value::Bool(false)]),
        ]
    );
}
