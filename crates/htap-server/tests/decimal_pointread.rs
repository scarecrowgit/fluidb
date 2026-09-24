use htap_common::types::{Row, Value};
use htap_server::LocalServer;
use htap_sql::result::StatementResult;
use tempfile::tempdir;

fn explain(server: &LocalServer, sql: &str) -> htap_sql::QueryResult {
    match server.execute(sql) {
        Ok(StatementResult::Query(result)) => result,
        Ok(other) => panic!("expected EXPLAIN query result for {sql}, got {other:?}"),
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn column_index(result: &htap_sql::QueryResult, name: &str) -> usize {
    result
        .columns
        .iter()
        .position(|column| column.name == name)
        .unwrap_or_else(|| panic!("missing {name} column"))
}

fn value(row: &Row, index: usize) -> &Value {
    row.get(index)
        .unwrap_or_else(|| panic!("missing column {index}"))
}

fn string_value(row: &Row, index: usize) -> &str {
    match value(row, index) {
        Value::String(value) => value,
        other => panic!("expected string at column {index}, got {other:?}"),
    }
}

fn int_value(row: &Row, index: usize) -> i64 {
    match value(row, index) {
        Value::Int64(value) => *value,
        other => panic!("expected int64 at column {index}, got {other:?}"),
    }
}

#[test]
fn test_decimal_primary_key_point_read_normalizes_scale() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute("CREATE TABLE decimal_keys (pk DECIMAL(10,2) PRIMARY KEY, label VARCHAR);")
        .unwrap();
    server
        .execute("INSERT INTO decimal_keys (pk, label) VALUES (5.50, 'five-fifty');")
        .unwrap();

    let result = match server.execute("SELECT pk FROM decimal_keys WHERE pk = 5.5;") {
        Ok(StatementResult::Query(result)) => result,
        Ok(other) => panic!("expected query result, got {other:?}"),
        Err(error) => panic!("decimal point read failed: {error}"),
    };

    assert_eq!(result.rows.len(), 1);
    match value(&result.rows[0], 0) {
        Value::Decimal {
            value,
            precision,
            scale,
        } => {
            assert_eq!(*value, 550);
            assert_eq!(*precision, 10);
            assert_eq!(*scale, 2);
        }
        other => panic!("expected decimal at column 0, got {other:?}"),
    }

    let plan = explain(
        &server,
        "EXPLAIN SELECT pk FROM decimal_keys WHERE pk = 5.5;",
    );
    let operation = column_index(&plan, "operation");

    assert_eq!(plan.rows.len(), 1);
    assert_eq!(string_value(&plan.rows[0], operation), "RowstorePointRead");

    let node_id = column_index(&plan, "node_id");
    assert!(int_value(&plan.rows[0], node_id) >= 0);
}

#[test]
fn test_decimal_value_survives_server_restart() {
    let dir = tempdir().unwrap();

    {
        let server = LocalServer::open(dir.path()).unwrap();
        server
            .execute(
                "CREATE TABLE decimal_durable \
                 (id INT PRIMARY KEY, amount DECIMAL(12,4));",
            )
            .unwrap();
        server
            .execute(
                "INSERT INTO decimal_durable (id, amount) \
                 VALUES (1, 12345678.9012);",
            )
            .unwrap();
    }

    let server = LocalServer::open(dir.path()).unwrap();
    let result = match server.execute("SELECT amount FROM decimal_durable WHERE id = 1;") {
        Ok(StatementResult::Query(result)) => result,
        Ok(other) => panic!("expected query result, got {other:?}"),
        Err(error) => panic!("decimal read after restart failed: {error}"),
    };

    assert_eq!(result.rows.len(), 1);
    match value(&result.rows[0], 0) {
        Value::Decimal {
            value,
            precision,
            scale,
        } => {
            assert_eq!(*value, 123_456_789_012);
            assert_eq!(*precision, 12);
            assert_eq!(*scale, 4);
        }
        other => panic!("expected decimal at column 0, got {other:?}"),
    }
}

#[test]
fn test_decimal_primary_key_range_scan_returns_numeric_order() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute("CREATE TABLE decimal_ranges (pk DECIMAL(10,2) PRIMARY KEY, label VARCHAR);")
        .unwrap();
    server
        .execute(
            "INSERT INTO decimal_ranges (pk, label) VALUES \
             (12.75, 'positive'), (-1.25, 'negative'), (0.00, 'zero'), (-10.50, 'smaller');",
        )
        .unwrap();

    let result = match server.execute("SELECT pk FROM decimal_ranges;") {
        Ok(StatementResult::Query(result)) => result,
        Ok(other) => panic!("expected query result, got {other:?}"),
        Err(error) => panic!("decimal range scan failed: {error}"),
    };

    let expected = [(-1050, 10, 2), (-125, 10, 2), (0, 10, 2), (1275, 10, 2)];
    assert_eq!(result.rows.len(), expected.len());

    for (row, (expected_value, expected_precision, expected_scale)) in
        result.rows.iter().zip(expected)
    {
        let Value::Decimal {
            value,
            precision,
            scale,
        } = value(row, 0)
        else {
            panic!("expected decimal primary key");
        };
        assert_eq!(*value, expected_value);
        assert_eq!(*precision, expected_precision);
        assert_eq!(*scale, expected_scale);
    }
}

#[test]
fn test_decimal_primary_key_collision_normalizes_textual_spellings() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute(
            "CREATE TABLE decimal_collisions \
             (pk DECIMAL(10,2) PRIMARY KEY, label VARCHAR);",
        )
        .unwrap();
    server
        .execute("INSERT INTO decimal_collisions (pk, label) VALUES (5.50, 'first');")
        .unwrap();
    server
        .execute("INSERT INTO decimal_collisions (pk, label) VALUES (5.5, 'second');")
        .unwrap();

    let result = match server.execute("SELECT pk FROM decimal_collisions;") {
        Ok(StatementResult::Query(result)) => result,
        Ok(other) => panic!("expected query result, got {other:?}"),
        Err(error) => panic!("decimal collision read failed: {error}"),
    };

    assert_eq!(result.rows.len(), 1);
    match value(&result.rows[0], 0) {
        Value::Decimal {
            value,
            precision,
            scale,
        } => {
            assert_eq!(*value, 550);
            assert_eq!(*precision, 10);
            assert_eq!(*scale, 2);
        }
        other => panic!("expected decimal at column 0, got {other:?}"),
    }
}
