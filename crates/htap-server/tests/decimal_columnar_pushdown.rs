//! Integration tests for exact DECIMAL predicate semantics across row and column execution.

use htap_common::types::{Row, Value};
use htap_server::LocalServer;
use htap_sql::result::StatementResult;
use tempfile::TempDir;

fn explain(server: &LocalServer, sql: &str) -> htap_sql::result::QueryResult {
    match server.execute(&format!("EXPLAIN {sql}")).unwrap() {
        StatementResult::Query(query) => query,
        other => panic!("expected EXPLAIN query result, got {other:?}"),
    }
}

fn column_index(query: &htap_sql::result::QueryResult, name: &str) -> usize {
    query
        .columns()
        .iter()
        .position(|column| column.name == name)
        .unwrap_or_else(|| panic!("expected column {name:?}, got {:?}", query.columns()))
}

fn string_value(row: &Row, index: usize) -> &str {
    match row.get(index) {
        Some(Value::String(value)) => value,
        other => panic!("expected string value at column {index}, got {other:?}"),
    }
}

fn assert_decimal_row(
    row: &Row,
    expected_id: i64,
    expected_unscaled: i64,
    expected_precision: u8,
    expected_scale: u8,
) {
    assert_eq!(row.get(0), Some(&Value::Int64(expected_id)));

    match row.get(1) {
        Some(Value::Decimal {
            value,
            precision,
            scale,
        }) => {
            assert_eq!(*value, expected_unscaled);
            assert_eq!(*precision, expected_precision);
            assert_eq!(*scale, expected_scale);
        }
        other => panic!("expected DECIMAL value, got {other:?}"),
    }
}

fn assert_single_decimal_query(
    result: StatementResult,
    expected_id: i64,
    expected_unscaled: i64,
    expected_precision: u8,
    expected_scale: u8,
) {
    match result {
        StatementResult::Query(query) => {
            assert_eq!(query.num_rows(), 1);
            assert_eq!(query.columns().len(), 2);
            assert_eq!(query.columns()[0].name, "id");
            assert_eq!(query.columns()[1].name, "value");
            assert_decimal_row(
                &query.rows()[0],
                expected_id,
                expected_unscaled,
                expected_precision,
                expected_scale,
            );
        }
        other => panic!("expected query result, got {other:?}"),
    }
}

#[test]
fn test_columnar_decimal_strict_greater_than_rewrites_to_rounded_up_boundary() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute("CREATE TABLE measurements (id BIGINT PRIMARY KEY, value DECIMAL(10, 2));")
        .unwrap();
    server
        .execute(
            "INSERT INTO measurements (id, value) VALUES \
             (1, 5.50), \
             (2, 5.56);",
        )
        .unwrap();

    server.convert_table_to_column("measurements").unwrap();

    let sql = "SELECT id, value FROM measurements \
               WHERE value > 5.555 ORDER BY id;";
    let plan = explain(&server, sql);
    let operation_index = column_index(&plan, "operation");
    assert!(
        plan.rows()
            .iter()
            .any(|row| string_value(row, operation_index).contains("OlapScan")),
        "expected OlapScan in EXPLAIN plan, got {:?}",
        plan.rows()
    );

    let result = server.execute(sql).unwrap();

    assert_single_decimal_query(result, 2, 556, 10, 2);
}

#[test]
fn test_columnar_decimal_strict_less_than_rewrites_to_rounded_down_boundary() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute("CREATE TABLE measurements (id BIGINT PRIMARY KEY, value DECIMAL(10, 2));")
        .unwrap();
    server
        .execute(
            "INSERT INTO measurements (id, value) VALUES \
             (1, 5.55), \
             (2, 5.60);",
        )
        .unwrap();

    server.convert_table_to_column("measurements").unwrap();

    let sql = "SELECT id, value FROM measurements \
               WHERE value < 5.555 ORDER BY id;";
    let plan = explain(&server, sql);
    let operation_index = column_index(&plan, "operation");
    assert!(
        plan.rows()
            .iter()
            .any(|row| string_value(row, operation_index).contains("OlapScan")),
        "expected OlapScan in EXPLAIN plan, got {:?}",
        plan.rows()
    );

    let result = server.execute(sql).unwrap();

    assert_single_decimal_query(result, 1, 555, 10, 2);
}

#[test]
fn test_decimal_non_representable_strict_boundaries_match_before_and_after_conversion() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute("CREATE TABLE measurements (id BIGINT PRIMARY KEY, value DECIMAL(10, 2));")
        .unwrap();
    server
        .execute(
            "INSERT INTO measurements (id, value) VALUES \
             (1, 5.50), \
             (2, 5.55), \
             (3, 5.56), \
             (4, 5.60);",
        )
        .unwrap();

    let row_greater = server
        .execute(
            "SELECT id, value FROM measurements \
             WHERE value > 5.555 ORDER BY id;",
        )
        .unwrap();
    match row_greater {
        StatementResult::Query(query) => {
            assert_eq!(query.num_rows(), 2);
            assert_eq!(query.columns().len(), 2);
            assert_decimal_row(&query.rows()[0], 3, 556, 10, 2);
            assert_decimal_row(&query.rows()[1], 4, 560, 10, 2);
        }
        other => panic!("expected query result, got {other:?}"),
    }

    let row_less = server
        .execute(
            "SELECT id, value FROM measurements \
             WHERE value < 5.555 ORDER BY id;",
        )
        .unwrap();
    match row_less {
        StatementResult::Query(query) => {
            assert_eq!(query.num_rows(), 2);
            assert_eq!(query.columns().len(), 2);
            assert_decimal_row(&query.rows()[0], 1, 550, 10, 2);
            assert_decimal_row(&query.rows()[1], 2, 555, 10, 2);
        }
        other => panic!("expected query result, got {other:?}"),
    }

    server.convert_table_to_column("measurements").unwrap();

    let column_greater = server
        .execute(
            "SELECT id, value FROM measurements \
             WHERE value > 5.555 ORDER BY id;",
        )
        .unwrap();
    match column_greater {
        StatementResult::Query(query) => {
            assert_eq!(query.num_rows(), 2);
            assert_eq!(query.columns().len(), 2);
            assert_decimal_row(&query.rows()[0], 3, 556, 10, 2);
            assert_decimal_row(&query.rows()[1], 4, 560, 10, 2);
        }
        other => panic!("expected query result, got {other:?}"),
    }

    let column_less = server
        .execute(
            "SELECT id, value FROM measurements \
             WHERE value < 5.555 ORDER BY id;",
        )
        .unwrap();
    match column_less {
        StatementResult::Query(query) => {
            assert_eq!(query.num_rows(), 2);
            assert_eq!(query.columns().len(), 2);
            assert_decimal_row(&query.rows()[0], 1, 550, 10, 2);
            assert_decimal_row(&query.rows()[1], 2, 555, 10, 2);
        }
        other => panic!("expected query result, got {other:?}"),
    }
}
