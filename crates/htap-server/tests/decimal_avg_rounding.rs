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
    result: &htap_sql::QueryResult,
    row: usize,
    column: usize,
    expected_unscaled: i64,
    expected_precision: u8,
    expected_scale: u8,
) {
    match value(result, row, column) {
        Value::Decimal {
            value,
            precision,
            scale,
        } => {
            assert_eq!(
                *value, expected_unscaled,
                "unexpected decimal value at row {row}, column {column}"
            );
            assert_eq!(
                *precision, expected_precision,
                "unexpected decimal precision at row {row}, column {column}"
            );
            assert_eq!(
                *scale, expected_scale,
                "unexpected decimal scale at row {row}, column {column}"
            );
        }
        other => panic!("expected decimal at row {row}, column {column}, got {other:?}"),
    }
}

fn create_measurements(server: &LocalServer) {
    server
        .execute(
            "CREATE TABLE measurements (
                id BIGINT PRIMARY KEY,
                group_id BIGINT,
                amount DECIMAL(3, 2)
            );",
        )
        .unwrap();
    server
        .execute(
            "INSERT INTO measurements (id, group_id, amount) VALUES
                (1, 1, 0.01),
                (2, 1, 0.01),
                (3, 1, 0.00),
                (4, 2, -0.01),
                (5, 2, -0.01),
                (6, 2, 0.00);",
        )
        .unwrap();
}

#[test]
fn test_decimal_avg_rounds_half_away_from_zero() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    create_measurements(&server);

    let result = query(
        &server,
        "SELECT
            group_id,
            AVG(amount)
         FROM measurements
         GROUP BY group_id
         ORDER BY group_id;",
    );

    assert_eq!(result.rows.len(), 2);

    assert_eq!(value(&result, 0, 0), &Value::Int64(1));
    assert_decimal(&result, 0, 1, 6667, 18, 6);

    assert_eq!(value(&result, 1, 0), &Value::Int64(2));
    assert_decimal(&result, 1, 1, -6667, 18, 6);
}

#[test]
fn test_decimal_window_avg_rounds_half_away_from_zero_for_every_partition_row() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    create_measurements(&server);

    let result = query(
        &server,
        "SELECT
            group_id,
            id,
            AVG(amount) OVER (PARTITION BY group_id)
         FROM measurements
         ORDER BY group_id, id;",
    );

    assert_eq!(result.rows.len(), 6);

    assert_eq!(value(&result, 0, 0), &Value::Int64(1));
    assert_eq!(value(&result, 0, 1), &Value::Int64(1));
    assert_decimal(&result, 0, 2, 6667, 18, 6);

    assert_eq!(value(&result, 1, 0), &Value::Int64(1));
    assert_eq!(value(&result, 1, 1), &Value::Int64(2));
    assert_decimal(&result, 1, 2, 6667, 18, 6);

    assert_eq!(value(&result, 2, 0), &Value::Int64(1));
    assert_eq!(value(&result, 2, 1), &Value::Int64(3));
    assert_decimal(&result, 2, 2, 6667, 18, 6);

    assert_eq!(value(&result, 3, 0), &Value::Int64(2));
    assert_eq!(value(&result, 3, 1), &Value::Int64(4));
    assert_decimal(&result, 3, 2, -6667, 18, 6);

    assert_eq!(value(&result, 4, 0), &Value::Int64(2));
    assert_eq!(value(&result, 4, 1), &Value::Int64(5));
    assert_decimal(&result, 4, 2, -6667, 18, 6);

    assert_eq!(value(&result, 5, 0), &Value::Int64(2));
    assert_eq!(value(&result, 5, 1), &Value::Int64(6));
    assert_decimal(&result, 5, 2, -6667, 18, 6);
}
