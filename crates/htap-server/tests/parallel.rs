use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use htap_common::types::Row;
use htap_server::LocalServer;
use htap_sql::result::StatementResult;

static NEXT_TEST_ROOT: AtomicU64 = AtomicU64::new(1);

fn test_root(name: &str) -> PathBuf {
    let id = NEXT_TEST_ROOT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("htap-server-{name}-{}-{id}", std::process::id()))
}

fn rows(server: &LocalServer, sql: &str) -> Vec<Row> {
    match server.execute(sql) {
        Ok(StatementResult::Query(query)) => query.rows,
        Ok(other) => panic!("expected query result for {sql}, got {other:?}"),
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn assert_parallel(server: &LocalServer, query: &str, expected: &[Row]) {
    let actual = rows(server, query);
    assert_eq!(actual, expected, "parallel execution changed query results");
    assert!(
        server.last_query_parallel_workers() > 1,
        "query did not use a parallel operator"
    );
}

#[test]
fn test_nested_parallel_stages_do_not_multiply_thread_count() {
    let root = test_root("nested-parallel-stages");
    let mut server = LocalServer::open(&root).expect("open server");

    server.set_query_parallelism(2);
    assert_eq!(server.query_parallelism(), 2);

    server
        .execute("CREATE TABLE left_rows (id BIGINT PRIMARY KEY, bucket BIGINT NOT NULL)")
        .expect("create left table");
    server
        .execute("CREATE TABLE right_rows (id BIGINT PRIMARY KEY)")
        .expect("create right table");
    for start in (1..=10_001_i64).step_by(500) {
        let end = (start + 499).min(10_001);
        let left_values = (start..=end)
            .map(|id| {
                let bucket = if id % 2 == 0 { 10 } else { 20 };
                format!("({id}, {bucket})")
            })
            .collect::<Vec<_>>()
            .join(", ");
        server
            .execute(&format!(
                "INSERT INTO left_rows (id, bucket) VALUES {left_values}"
            ))
            .expect("insert left rows");
        let right_values = (start..=end)
            .map(|id| format!("({id})"))
            .collect::<Vec<_>>()
            .join(", ");
        server
            .execute(&format!(
                "INSERT INTO right_rows (id) VALUES {right_values}"
            ))
            .expect("insert right rows");
    }

    let query = "SELECT l.bucket, COUNT(*) \
                 FROM left_rows AS l \
                 JOIN right_rows AS r ON l.id = r.id \
                 GROUP BY l.bucket \
                 ORDER BY l.bucket";
    server.set_query_parallelism(1);
    let expected = rows(&server, query);
    server.set_query_parallelism(2);
    assert_parallel(&server, query, &expected);

    // Nested stages share the statement's one configured degree of parallelism.
    assert_eq!(server.query_parallelism(), 2);

    drop(server);
    std::fs::remove_dir_all(root).expect("remove test root");
}

#[test]
fn test_inner_join_parallel_determinism_across_worker_counts() {
    let root = test_root("inner-join-parallel-determinism");
    let mut server = LocalServer::open(&root).expect("open server");

    server
        .execute("CREATE TABLE join_left (id BIGINT PRIMARY KEY, join_key BIGINT NOT NULL)")
        .expect("create left table");
    server
        .execute("CREATE TABLE join_right (id BIGINT PRIMARY KEY, join_key BIGINT NOT NULL)")
        .expect("create right table");

    for start in (1..=257_i64).step_by(500) {
        let end = (start + 499).min(257);
        let values = (start..=end)
            .map(|id| {
                let join_key = (id * 17) % 23;
                format!("({id}, {join_key})")
            })
            .collect::<Vec<_>>()
            .join(", ");
        server
            .execute(&format!(
                "INSERT INTO join_left (id, join_key) VALUES {values}"
            ))
            .expect("insert left rows");
    }
    for start in (1..=10_001_i64).step_by(500) {
        let end = (start + 499).min(10_001);
        let values = (start..=end)
            .map(|id| {
                let join_key = (id * 13) % 23;
                format!("({id}, {join_key})")
            })
            .collect::<Vec<_>>()
            .join(", ");
        server
            .execute(&format!(
                "INSERT INTO join_right (id, join_key) VALUES {values}"
            ))
            .expect("insert right rows");
    }

    let query = "SELECT l.id, l.join_key, r.id \
                 FROM join_left AS l \
                 JOIN join_right AS r ON l.join_key = r.join_key";
    let available = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .max(2);
    let worker_counts = [1, 2, available];

    server.set_query_parallelism(worker_counts[0]);
    let expected = rows(&server, query);

    for &worker_count in &worker_counts {
        server.set_query_parallelism(worker_count);
        if worker_count > 1 {
            assert_parallel(&server, query, &expected);
        } else {
            assert_eq!(
                rows(&server, query),
                expected,
                "serial execution changed query results"
            );
        }
    }

    drop(server);
    std::fs::remove_dir_all(root).expect("remove test root");
}

#[test]
fn test_inner_join_parallel_matches_serial_row_order() {
    let root = test_root("inner-join-parallel-row-order");
    let mut server = LocalServer::open(&root).expect("open server");

    server
        .execute("CREATE TABLE join_left (id BIGINT PRIMARY KEY, join_key BIGINT NOT NULL)")
        .expect("create left table");
    server
        .execute("CREATE TABLE join_right (id BIGINT PRIMARY KEY, join_key BIGINT NOT NULL)")
        .expect("create right table");
    server
        .execute(
            "INSERT INTO join_left (id, join_key) VALUES \
             (1, 20), (2, 10), (3, 30), (4, 10), (5, 20), (6, 40)",
        )
        .expect("insert left rows");
    server
        .execute(
            "INSERT INTO join_right (id, join_key) VALUES \
             (101, 10), (102, 20), (103, 30), (104, 40)",
        )
        .expect("insert right rows");
    for start in (105..=10_104_i64).step_by(500) {
        let end = (start + 499).min(10_104);
        let values = (start..=end)
            .map(|id| format!("({id}, {})", id + 100_000))
            .collect::<Vec<_>>()
            .join(", ");
        server
            .execute(&format!(
                "INSERT INTO join_right (id, join_key) VALUES {values}"
            ))
            .expect("insert extra right rows");
    }

    let query = "SELECT l.id \
                 FROM join_left AS l \
                 JOIN join_right AS r ON l.join_key = r.join_key";

    server.set_query_parallelism(1);
    let serial = rows(&server, query);

    server.set_query_parallelism(2);
    assert_parallel(&server, query, &serial);

    assert_eq!(
        serial,
        rows(&server, "SELECT id FROM join_left"),
        "join rows did not follow their source left rows"
    );

    drop(server);
    std::fs::remove_dir_all(root).expect("remove test root");
}

#[test]
fn test_inner_join_parallel_key_normalization_consistency() {
    let root = test_root("inner-join-parallel-key-normalization");
    let mut server = LocalServer::open(&root).expect("open server");

    server
        .execute("CREATE TABLE integer_keys (id BIGINT PRIMARY KEY, join_key BIGINT NOT NULL)")
        .expect("create integer-key table");
    server
        .execute("CREATE TABLE float_keys (id BIGINT PRIMARY KEY, join_key DOUBLE NOT NULL)")
        .expect("create floating-point-key table");
    server
        .execute(
            "INSERT INTO integer_keys (id, join_key) VALUES \
             (1, -7), (2, 0), (3, 1), (4, 42), (5, 9007199254740992)",
        )
        .expect("insert integer keys");
    server
        .execute(
            "INSERT INTO float_keys (id, join_key) VALUES \
             (101, -7.0), (102, 0.0), (103, 1.0), (104, 42.0), \
             (105, 9007199254740992.0), (106, 3.5)",
        )
        .expect("insert floating-point keys");
    for start in (107..=10_106_i64).step_by(500) {
        let end = (start + 499).min(10_106);
        let values = (start..=end)
            .map(|id| format!("({id}, {}.5)", id + 100_000))
            .collect::<Vec<_>>()
            .join(", ");
        server
            .execute(&format!(
                "INSERT INTO float_keys (id, join_key) VALUES {values}"
            ))
            .expect("insert extra floating-point keys");
    }

    let query = "SELECT i.id, f.id \
                 FROM integer_keys AS i \
                 JOIN float_keys AS f ON i.join_key = f.join_key";
    server.set_query_parallelism(1);
    let expected = rows(&server, query);

    let available = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .max(2);
    for worker_count in [2, available] {
        server.set_query_parallelism(worker_count);
        assert_parallel(&server, query, &expected);
    }

    assert_eq!(expected.len(), 5, "not all equivalent numeric keys matched");

    drop(server);
    std::fs::remove_dir_all(root).expect("remove test root");
}

#[test]
fn test_group_by_parallel_determinism_across_worker_counts() {
    let root = test_root("group-by-parallel-determinism");
    let mut server = LocalServer::open(&root).expect("open server");

    server
        .execute(
            "CREATE TABLE events (\
                id BIGINT PRIMARY KEY, \
                bucket BIGINT NOT NULL, \
                amount BIGINT NOT NULL\
            )",
        )
        .expect("create events table");

    for start in (1..=10_001_i64).step_by(500) {
        let end = (start + 499).min(10_001);
        let values = (start..=end)
            .map(|id| {
                let bucket = (id * 17) % 11;
                let amount = (id * 13) % 101;
                format!("({id}, {bucket}, {amount})")
            })
            .collect::<Vec<_>>()
            .join(", ");
        server
            .execute(&format!(
                "INSERT INTO events (id, bucket, amount) VALUES {values}"
            ))
            .expect("insert events");
    }

    // COUNT(DISTINCT ...) is rejected by the narrow analytic binder, so this
    // query shape takes the general executor's parallel GROUP BY path.
    let query = "SELECT bucket, COUNT(*), SUM(amount), MIN(amount), MAX(amount), \
                 COUNT(DISTINCT id) \
                 FROM events \
                 GROUP BY bucket \
                 ORDER BY bucket";
    let available = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .max(2);
    let worker_counts = [1, 2, available];

    server.set_query_parallelism(worker_counts[0]);
    let expected = rows(&server, query);

    for &worker_count in &worker_counts {
        server.set_query_parallelism(worker_count);
        if worker_count > 1 {
            assert_parallel(&server, query, &expected);
        } else {
            assert_eq!(
                rows(&server, query),
                expected,
                "serial execution changed query results"
            );
        }
    }

    drop(server);
    std::fs::remove_dir_all(root).expect("remove test root");
}

#[test]
fn test_group_by_parallel_avg_and_distinct_correct() {
    let root = test_root("group-by-parallel-avg-distinct");
    let mut server = LocalServer::open(&root).expect("open server");

    server
        .execute(
            "CREATE TABLE measurements (\
                id BIGINT PRIMARY KEY, \
                bucket BIGINT NOT NULL, \
                value BIGINT NOT NULL, \
                tag BIGINT NOT NULL\
            )",
        )
        .expect("create measurements table");

    let measurements = [
        (1, 1, 1, 10),
        (2, 1, 3, 10),
        (3, 1, 5, 20),
        (4, 1, 7, 20),
        (5, 1, 100, 30),
        (6, 2, 2, 40),
        (7, 2, 4, 40),
        (8, 2, 6, 50),
        (9, 3, 9, 60),
        (10, 3, 9, 60),
        (11, 3, 12, 70),
    ];
    let values = measurements
        .iter()
        .map(|(id, bucket, value, tag)| format!("({id}, {bucket}, {value}, {tag})"))
        .collect::<Vec<_>>()
        .join(", ");
    server
        .execute(&format!(
            "INSERT INTO measurements (id, bucket, value, tag) VALUES {values}"
        ))
        .expect("insert measurements");
    for start in (12..=10_011_i64).step_by(500) {
        let end = (start + 499).min(10_011);
        let values = (start..=end)
            .map(|id| {
                let bucket = (id * 17) % 3 + 1;
                let value = (id * 13) % 101;
                let tag = (id * 19) % 23 + 100;
                format!("({id}, {bucket}, {value}, {tag})")
            })
            .collect::<Vec<_>>()
            .join(", ");
        server
            .execute(&format!(
                "INSERT INTO measurements (id, bucket, value, tag) VALUES {values}"
            ))
            .expect("insert extra measurements");
    }

    let query = "SELECT bucket, AVG(value), COUNT(DISTINCT tag), SUM(DISTINCT tag) \
                 FROM measurements \
                 GROUP BY bucket \
                 ORDER BY bucket";
    let available = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .max(2);
    let worker_counts = [1, 2, available];

    server.set_query_parallelism(1);
    let expected = rows(&server, query);

    for &worker_count in &worker_counts {
        server.set_query_parallelism(worker_count);
        if worker_count > 1 {
            assert_parallel(&server, query, &expected);
        } else {
            assert_eq!(
                rows(&server, query),
                expected,
                "serial execution changed query results"
            );
        }
    }

    drop(server);
    std::fs::remove_dir_all(root).expect("remove test root");
}

#[test]
fn test_group_by_with_variable_does_not_parallelize() {
    let root = test_root("group-by-with-variable");
    let mut server = LocalServer::open(&root).expect("open server");

    server
        .execute(
            "CREATE TABLE variable_events (\
                id BIGINT PRIMARY KEY, \
                bucket BIGINT NOT NULL\
            )",
        )
        .expect("create variable events table");

    for start in (1..=10_001_i64).step_by(500) {
        let end = (start + 499).min(10_001);
        let values = (start..=end)
            .map(|id| {
                let bucket = id % 11;
                format!("({id}, {bucket})")
            })
            .collect::<Vec<_>>()
            .join(", ");
        server
            .execute(&format!(
                "INSERT INTO variable_events (id, bucket) VALUES {values}"
            ))
            .expect("insert variable events");
    }

    server.set_query_parallelism(2);

    let query = "SELECT bucket, @@version, COUNT(*) \
                 FROM variable_events \
                 GROUP BY bucket, @@version \
                 ORDER BY bucket";
    let result = rows(&server, query);

    assert_eq!(result.len(), 11);
    assert_eq!(
        server.last_query_parallel_workers(),
        1,
        "GROUP BY using a variable should not parallelize"
    );

    drop(server);
    std::fs::remove_dir_all(root).expect("remove test root");
}
