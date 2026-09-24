use htap_common::types::{Row, Value};
use htap_server::LocalServer;
use htap_sql::result::StatementResult;
use tempfile::tempdir;

fn query(server: &LocalServer, sql: &str) -> htap_sql::QueryResult {
    match server.execute(sql) {
        Ok(StatementResult::Query(result)) => result,
        Ok(other) => panic!("expected query result for {sql}, got {other:?}"),
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn value(row: &Row, index: usize) -> &Value {
    row.get(index)
        .unwrap_or_else(|| panic!("missing column {index}"))
}

fn setup_server() -> LocalServer {
    let dir = tempdir().unwrap();
    let path = dir.keep();
    let server = LocalServer::open(&path).unwrap();

    server
        .execute("CREATE TABLE decimal_values (id INT PRIMARY KEY, value DECIMAL(2,0));")
        .unwrap();
    server
        .execute(
            "INSERT INTO decimal_values (id, value) VALUES \
             (1, -99), (2, -1), (3, 0), (4, 1), (5, 99), (6, NULL);",
        )
        .unwrap();

    server
}

fn assert_decimal_rows(result: &htap_sql::QueryResult, expected: &[(i32, i64)]) {
    assert_eq!(result.rows.len(), expected.len());

    for (row, (expected_id, expected_decimal)) in result.rows.iter().zip(expected) {
        assert_eq!(value(row, 0), &Value::Int32(*expected_id));

        match value(row, 1) {
            Value::Decimal {
                value,
                precision,
                scale,
            } => {
                assert_eq!(*value, *expected_decimal);
                assert_eq!(*precision, 2);
                assert_eq!(*scale, 0);
            }
            other => panic!("expected decimal at column 1, got {other:?}"),
        }
    }
}

#[test]
fn test_decimal_comparisons_above_representable_range() {
    let server = setup_server();

    assert_decimal_rows(
        &query(
            &server,
            "SELECT id, value FROM decimal_values WHERE value < 100;",
        ),
        &[(1, -99), (2, -1), (3, 0), (4, 1), (5, 99)],
    );
    assert_decimal_rows(
        &query(
            &server,
            "SELECT id, value FROM decimal_values WHERE value <= 100;",
        ),
        &[(1, -99), (2, -1), (3, 0), (4, 1), (5, 99)],
    );
    assert_decimal_rows(
        &query(
            &server,
            "SELECT id, value FROM decimal_values WHERE value > 100;",
        ),
        &[],
    );
    assert_decimal_rows(
        &query(
            &server,
            "SELECT id, value FROM decimal_values WHERE value >= 100;",
        ),
        &[],
    );
    assert_decimal_rows(
        &query(
            &server,
            "SELECT id, value FROM decimal_values WHERE value = 100;",
        ),
        &[],
    );
    assert_decimal_rows(
        &query(
            &server,
            "SELECT id, value FROM decimal_values WHERE value <> 100;",
        ),
        &[(1, -99), (2, -1), (3, 0), (4, 1), (5, 99)],
    );
}

#[test]
fn test_decimal_comparisons_below_representable_range() {
    let server = setup_server();

    assert_decimal_rows(
        &query(
            &server,
            "SELECT id, value FROM decimal_values WHERE value < -100;",
        ),
        &[],
    );
    assert_decimal_rows(
        &query(
            &server,
            "SELECT id, value FROM decimal_values WHERE value <= -100;",
        ),
        &[],
    );
    assert_decimal_rows(
        &query(
            &server,
            "SELECT id, value FROM decimal_values WHERE value > -100;",
        ),
        &[(1, -99), (2, -1), (3, 0), (4, 1), (5, 99)],
    );
    assert_decimal_rows(
        &query(
            &server,
            "SELECT id, value FROM decimal_values WHERE value >= -100;",
        ),
        &[(1, -99), (2, -1), (3, 0), (4, 1), (5, 99)],
    );
    assert_decimal_rows(
        &query(
            &server,
            "SELECT id, value FROM decimal_values WHERE value = -100;",
        ),
        &[],
    );
    assert_decimal_rows(
        &query(
            &server,
            "SELECT id, value FROM decimal_values WHERE value <> -100;",
        ),
        &[(1, -99), (2, -1), (3, 0), (4, 1), (5, 99)],
    );
}

#[test]
fn test_decimal_comparisons_at_representable_boundary() {
    let server = setup_server();

    assert_decimal_rows(
        &query(
            &server,
            "SELECT id, value FROM decimal_values WHERE value < 99;",
        ),
        &[(1, -99), (2, -1), (3, 0), (4, 1)],
    );
    assert_decimal_rows(
        &query(
            &server,
            "SELECT id, value FROM decimal_values WHERE value <= 99;",
        ),
        &[(1, -99), (2, -1), (3, 0), (4, 1), (5, 99)],
    );
    assert_decimal_rows(
        &query(
            &server,
            "SELECT id, value FROM decimal_values WHERE value > 99;",
        ),
        &[],
    );
    assert_decimal_rows(
        &query(
            &server,
            "SELECT id, value FROM decimal_values WHERE value >= 99;",
        ),
        &[(5, 99)],
    );
    assert_decimal_rows(
        &query(
            &server,
            "SELECT id, value FROM decimal_values WHERE value = 99;",
        ),
        &[(5, 99)],
    );
    assert_decimal_rows(
        &query(
            &server,
            "SELECT id, value FROM decimal_values WHERE value <> 99;",
        ),
        &[(1, -99), (2, -1), (3, 0), (4, 1)],
    );
}
