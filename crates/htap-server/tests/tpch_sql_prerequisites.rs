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

#[test]
fn test_derived_table_column_list_renames_and_filters() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    let result = query(
        &server,
        "SELECT order_id
         FROM (
             SELECT 1, DATE '1998-12-31'
         ) AS orders(order_id, order_date)
         WHERE order_date = DATE '1998-12-31';",
    );

    assert_eq!(result.rows.len(), 1);
    assert_eq!(value(&result, 0, 0), &Value::Int64(1));
}

#[test]
fn test_derived_table_column_list_count_mismatch_error() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    let error = server
        .execute("SELECT * FROM (SELECT 1, 2) AS derived(value);")
        .unwrap_err();

    assert!(error.to_string().contains("column"));
}

#[test]
fn test_derived_table_column_list_duplicate_name_error() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    let error = server
        .execute("SELECT * FROM (SELECT 1, 2) AS derived(value, value);")
        .unwrap_err();

    assert!(error.to_string().contains("duplicate"));
}

#[test]
fn test_date_function_produces_same_result_as_literal() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    let result = query(&server, "SELECT DATE('1998-12-31'), DATE '1998-12-31';");

    assert_eq!(result.rows.len(), 1);
    assert_eq!(
        value(&result, 0, 0),
        &Value::Timestamp(parse_date_to_timestamp_micros("1998-12-31").unwrap())
    );
    assert_eq!(value(&result, 0, 0), value(&result, 0, 1));
}

#[test]
fn test_date_function_invalid_date_rejected() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    let error = server.execute("SELECT DATE('1998-02-30');").unwrap_err();

    assert!(error.to_string().contains("date"));
}

#[test]
fn test_date_function_and_literal_mixed_in_query() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute(
            "CREATE TABLE orders (
                id BIGINT PRIMARY KEY,
                order_date DATE
            );",
        )
        .unwrap();
    server
        .execute(
            "INSERT INTO orders (id, order_date) VALUES
                (1, DATE '1998-12-31'),
                (2, DATE '1999-01-31');",
        )
        .unwrap();

    let result = query(
        &server,
        "SELECT id
         FROM orders
         WHERE order_date >= DATE('1999-01-01')
           AND order_date < DATE '1999-02-01';",
    );

    assert_eq!(result.rows.len(), 1);
    assert_eq!(value(&result, 0, 0), &Value::Int64(2));
}

#[test]
fn test_cte_column_list_renames_and_filters() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    let result = query(
        &server,
        "WITH derived(c1, c2) AS (
             SELECT 1, DATE '1998-12-31'
         )
         SELECT c1
         FROM derived
         WHERE c2 = DATE '1998-12-31';",
    );

    assert_eq!(result.rows.len(), 1);
    assert_eq!(value(&result, 0, 0), &Value::Int64(1));
}

#[test]
fn test_cte_referenced_twice_with_column_list() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    let result = query(
        &server,
        "WITH revenue(supplier_id, total_revenue) AS (
             SELECT 1, 100
         )
         SELECT left_revenue.supplier_id, left_revenue.total_revenue
         FROM revenue AS left_revenue
         JOIN revenue AS right_revenue
           ON left_revenue.supplier_id = right_revenue.supplier_id;",
    );

    assert_eq!(result.rows.len(), 1);
    assert_eq!(value(&result, 0, 0), &Value::Int64(1));
    assert_eq!(value(&result, 0, 1), &Value::Int64(100));
}

#[test]
fn test_cte_column_list_too_few_names_error() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    let error = server
        .execute("WITH derived(value) AS (SELECT 1, 2) SELECT * FROM derived;")
        .unwrap_err();

    assert!(error.to_string().contains("column"));
    assert!(error.to_string().contains("derived"));
}

#[test]
fn test_cte_column_list_too_many_names_error() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    let error = server
        .execute(
            "WITH derived(first_value, second_value, third_value) AS (
                 SELECT 1, 2
             )
             SELECT * FROM derived;",
        )
        .unwrap_err();

    assert!(error.to_string().contains("column"));
    assert!(error.to_string().contains("derived"));
}

#[test]
fn test_cte_column_list_duplicate_name_error() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    let error = server
        .execute(
            "WITH derived(value, value) AS (SELECT 1, 2)
             SELECT * FROM derived;",
        )
        .unwrap_err();

    assert!(error.to_string().contains("duplicate"));
}

#[test]
fn test_recursive_cte_with_column_list_regression() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    let result = query(
        &server,
        "WITH RECURSIVE numbers(value) AS (
             SELECT 1
             UNION ALL
             SELECT value + 1 FROM numbers WHERE value < 3
         )
         SELECT value FROM numbers;",
    );

    assert_eq!(result.rows.len(), 3);
    assert_eq!(value(&result, 0, 0), &Value::Int64(1));
    assert_eq!(value(&result, 1, 0), &Value::Int64(2));
    assert_eq!(value(&result, 2, 0), &Value::Int64(3));
}
