use htap_common::types::{parse_date_to_timestamp_micros, Value};
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

#[test]
fn test_tpch_sql_prerequisites_parse_bind_and_execute_dates_and_strings() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute(
            "CREATE TABLE orders (
                id BIGINT PRIMARY KEY,
                order_date DATE,
                status CHAR(1),
                comment VARCHAR
            );",
        )
        .unwrap();
    server
        .execute(
            "INSERT INTO orders (id, order_date, status, comment) VALUES
                (1, DATE '1998-12-31', 'O', 'priority shipment'),
                (2, DATE '1999-01-31', 'F', 'regular order');",
        )
        .unwrap();

    let dates_and_chars = query(
        &server,
        "SELECT order_date, status FROM orders WHERE id = 1;",
    );
    assert_eq!(dates_and_chars.rows.len(), 1);
    assert_eq!(
        value(&dates_and_chars, 0, 0),
        &Value::Timestamp(parse_date_to_timestamp_micros("1998-12-31").unwrap())
    );
    assert_eq!(value(&dates_and_chars, 0, 1), &Value::String("O".into()));

    let date_filter = query(
        &server,
        "SELECT id FROM orders WHERE order_date > DATE '1998-12-31';",
    );
    assert_eq!(date_filter.rows.len(), 1);
    assert_eq!(value(&date_filter, 0, 0), &Value::Int64(2));

    let date_arithmetic = query(
        &server,
        "SELECT
            DATE '1999-01-31' + INTERVAL '1' MONTH,
            DATE '1999-01-01' - INTERVAL '1' DAY;",
    );
    assert_eq!(date_arithmetic.rows.len(), 1);
    assert_eq!(
        value(&date_arithmetic, 0, 0),
        &Value::Timestamp(parse_date_to_timestamp_micros("1999-02-28").unwrap())
    );
    assert_eq!(
        value(&date_arithmetic, 0, 1),
        &Value::Timestamp(parse_date_to_timestamp_micros("1998-12-31").unwrap())
    );

    let date_parts = query(
        &server,
        "SELECT
            EXTRACT(YEAR FROM order_date),
            EXTRACT(MONTH FROM order_date),
            EXTRACT(DAY FROM order_date)
         FROM orders
         WHERE id = 1;",
    );
    assert_eq!(date_parts.rows.len(), 1);
    assert_eq!(value(&date_parts, 0, 0), &Value::Int64(1998));
    assert_eq!(value(&date_parts, 0, 1), &Value::Int64(12));
    assert_eq!(value(&date_parts, 0, 2), &Value::Int64(31));

    let substring = query(
        &server,
        "SELECT SUBSTRING(comment FROM 10 FOR 20) FROM orders WHERE id = 1;",
    );
    assert_eq!(substring.rows.len(), 1);
    assert_eq!(value(&substring, 0, 0), &Value::String("shipment".into()));
}
