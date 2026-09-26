use htap_common::types::Value;
use htap_server::LocalServer;
use htap_sql::result::StatementResult;

fn server_with_rows() -> (tempfile::TempDir, LocalServer) {
    let root = tempfile::tempdir().expect("temporary root creates");
    let server = LocalServer::open(root.path()).expect("server opens");
    server
        .execute("CREATE TABLE limit_rows (id INT NOT NULL PRIMARY KEY, v INT NOT NULL)")
        .expect("table creates");
    server
        .execute("INSERT INTO limit_rows (id, v) VALUES (1, 30), (2, 10), (3, 40), (4, 20)")
        .expect("test data inserts");
    (root, server)
}

fn query_values(server: &LocalServer, sql: &str) -> Vec<Vec<Value>> {
    let result = server.execute(sql).expect("query succeeds");
    let StatementResult::Query(query) = result else {
        panic!("expected query result, got {result:?}");
    };
    query.rows.iter().map(|row| row.values().to_vec()).collect()
}

#[test]
fn limit_returns_only_the_requested_number_of_rows() {
    let (_root, server) = server_with_rows();

    let rows = query_values(&server, "SELECT id, v FROM limit_rows ORDER BY id LIMIT 2");

    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows,
        vec![
            vec![Value::Int32(1), Value::Int32(30)],
            vec![Value::Int32(2), Value::Int32(10)],
        ]
    );
}

#[test]
fn limit_applies_after_ordering() {
    let (_root, server) = server_with_rows();

    let rows = query_values(&server, "SELECT v FROM limit_rows ORDER BY v LIMIT 2");

    // Insertion order starts with 30, 10; truncating before sorting would return 10, 30.
    assert_eq!(rows, vec![vec![Value::Int32(10)], vec![Value::Int32(20)]]);
}

#[test]
fn limit_larger_than_result_returns_all_rows() {
    let (_root, server) = server_with_rows();

    let rows = query_values(&server, "SELECT id, v FROM limit_rows ORDER BY id LIMIT 10");

    assert_eq!(
        rows,
        vec![
            vec![Value::Int32(1), Value::Int32(30)],
            vec![Value::Int32(2), Value::Int32(10)],
            vec![Value::Int32(3), Value::Int32(40)],
            vec![Value::Int32(4), Value::Int32(20)],
        ]
    );
}

#[test]
fn limit_zero_returns_no_rows() {
    let (_root, server) = server_with_rows();

    let rows = query_values(&server, "SELECT id, v FROM limit_rows ORDER BY id LIMIT 0");

    assert_eq!(rows, Vec::<Vec<Value>>::new());
}

#[test]
fn limit_with_offset_skips_rows_before_truncating() {
    let (_root, server) = server_with_rows();

    let rows = query_values(
        &server,
        "SELECT v FROM limit_rows ORDER BY v LIMIT 2 OFFSET 1",
    );

    assert_eq!(rows, vec![vec![Value::Int32(20)], vec![Value::Int32(30)]]);
}
