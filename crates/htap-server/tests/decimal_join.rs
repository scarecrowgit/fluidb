use htap_common::types::Value;
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

fn value(result: &htap_sql::QueryResult, row: usize, column: usize) -> &Value {
    result.rows[row]
        .get(column)
        .unwrap_or_else(|| panic!("missing value at row {row}, column {column}"))
}

fn assert_decimal(
    value: &Value,
    expected_unscaled: i64,
    expected_precision: u8,
    expected_scale: u8,
) {
    let Value::Decimal {
        value: unscaled,
        precision,
        scale,
    } = value
    else {
        panic!("expected decimal value, got {value:?}");
    };

    assert_eq!(*unscaled, expected_unscaled);
    assert_eq!(*precision, expected_precision);
    assert_eq!(*scale, expected_scale);
}

#[test]
fn test_decimal_to_decimal_equi_join() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute(
            "CREATE TABLE orders (
                id BIGINT PRIMARY KEY,
                amount DECIMAL(10, 2)
            );",
        )
        .unwrap();
    server
        .execute(
            "CREATE TABLE payments (
                id BIGINT PRIMARY KEY,
                amount DECIMAL(10, 2)
            );",
        )
        .unwrap();

    server
        .execute(
            "INSERT INTO orders (id, amount) VALUES
                (1, 12.34),
                (2, 56.78),
                (3, 90.12);",
        )
        .unwrap();
    server
        .execute(
            "INSERT INTO payments (id, amount) VALUES
                (10, 56.78),
                (11, 12.34),
                (12, 99.99);",
        )
        .unwrap();

    let result = query(
        &server,
        "SELECT orders.id, payments.id, orders.amount
         FROM orders
         JOIN payments ON orders.amount = payments.amount
         ORDER BY orders.id;",
    );

    assert_eq!(result.rows.len(), 2);

    assert_eq!(value(&result, 0, 0), &Value::Int64(1));
    assert_eq!(value(&result, 0, 1), &Value::Int64(11));
    assert_decimal(value(&result, 0, 2), 1234, 10, 2);

    assert_eq!(value(&result, 1, 0), &Value::Int64(2));
    assert_eq!(value(&result, 1, 1), &Value::Int64(10));
    assert_decimal(value(&result, 1, 2), 5678, 10, 2);
}

#[test]
fn test_decimal_to_integer_equi_join() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute(
            "CREATE TABLE decimal_values (
                id BIGINT PRIMARY KEY,
                amount DECIMAL(10, 0)
            );",
        )
        .unwrap();
    server
        .execute(
            "CREATE TABLE integer_values (
                id BIGINT PRIMARY KEY,
                amount BIGINT
            );",
        )
        .unwrap();

    server
        .execute(
            "INSERT INTO decimal_values (id, amount) VALUES
                (1, 7),
                (2, 42),
                (3, 100);",
        )
        .unwrap();
    server
        .execute(
            "INSERT INTO integer_values (id, amount) VALUES
                (10, 42),
                (11, 7),
                (12, 99);",
        )
        .unwrap();

    let result = query(
        &server,
        "SELECT decimal_values.id, integer_values.id, decimal_values.amount
         FROM decimal_values
         JOIN integer_values ON decimal_values.amount = integer_values.amount
         ORDER BY decimal_values.id;",
    );

    assert_eq!(result.rows.len(), 2);

    assert_eq!(value(&result, 0, 0), &Value::Int64(1));
    assert_eq!(value(&result, 0, 1), &Value::Int64(11));
    assert_decimal(value(&result, 0, 2), 7, 10, 0);

    assert_eq!(value(&result, 1, 0), &Value::Int64(2));
    assert_eq!(value(&result, 1, 1), &Value::Int64(10));
    assert_decimal(value(&result, 1, 2), 42, 10, 0);
}
