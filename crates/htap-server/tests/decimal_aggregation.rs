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

fn scalar(server: &LocalServer, sql: &str) -> Value {
    let result = query(server, sql);
    assert_eq!(result.rows.len(), 1, "expected one row for {sql}");
    value(&result, 0, 0).clone()
}

#[test]
fn test_tpch_style_money_aggregation_uses_exact_decimal_precision() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute(
            "CREATE TABLE lineitems (
                id BIGINT PRIMARY KEY,
                order_key BIGINT,
                price_cents BIGINT,
                discount_percent BIGINT
            );",
        )
        .unwrap();
    server
        .execute(
            "INSERT INTO lineitems (id, order_key, price_cents, discount_percent) VALUES
                (1, 1, 10000, 5),
                (2, 1, 10000, 10),
                (3, 2, 10, 0),
                (4, 2, 10, 0),
                (5, 2, 10, 0);",
        )
        .unwrap();

    let result = query(
        &server,
        "SELECT
            order_key,
            SUM(price_cents * 0.01 * (1.00 - discount_percent * 0.01))
         FROM lineitems
         GROUP BY order_key
         ORDER BY order_key;",
    );

    assert_eq!(result.rows.len(), 2);
    assert_eq!(value(&result, 0, 0), &Value::Int64(1));

    let Value::Decimal {
        value: actual_value,
        precision: actual_precision,
        scale: actual_scale,
    } = value(&result, 0, 1)
    else {
        panic!("expected decimal result for order_key = 1");
    };
    let expected = scalar(&server, "SELECT CAST(185.0000 AS DECIMAL(18, 4));");
    let Value::Decimal {
        value: expected_value,
        precision: expected_precision,
        scale: expected_scale,
    } = expected
    else {
        panic!("expected decimal scalar for 185.0000");
    };
    assert_eq!(actual_value, &expected_value);
    assert_eq!(actual_precision, &expected_precision);
    assert_eq!(actual_scale, &expected_scale);

    assert_eq!(value(&result, 1, 0), &Value::Int64(2));

    let Value::Decimal {
        value: actual_value,
        precision: actual_precision,
        scale: actual_scale,
    } = value(&result, 1, 1)
    else {
        panic!("expected decimal result for order_key = 2");
    };
    let expected = scalar(&server, "SELECT CAST(0.3000 AS DECIMAL(18, 4));");
    let Value::Decimal {
        value: expected_value,
        precision: expected_precision,
        scale: expected_scale,
    } = expected
    else {
        panic!("expected decimal scalar for 0.3000");
    };
    assert_eq!(actual_value, &expected_value);
    assert_eq!(actual_precision, &expected_precision);
    assert_eq!(actual_scale, &expected_scale);

    let floating_point_result = query(
        &server,
        "SELECT
            SUM(
                CAST(price_cents AS DOUBLE) * 0.01
                * (1.0 - CAST(discount_percent AS DOUBLE) * 0.01)
            )
         FROM lineitems
         WHERE order_key = 2;",
    );
    assert_eq!(floating_point_result.rows.len(), 1);
    assert_ne!(
        value(&floating_point_result, 0, 0),
        value(&result, 1, 1),
        "the floating-point calculation must not replace the exact decimal result"
    );
}

#[test]
fn test_decimal_sum_avg_min_max_and_distinct_count() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute(
            "CREATE TABLE measurements (
                id BIGINT PRIMARY KEY,
                amount_cents BIGINT
            );",
        )
        .unwrap();
    server
        .execute(
            "INSERT INTO measurements (id, amount_cents) VALUES
                (1, 3000),
                (2, 3000),
                (3, 4000);",
        )
        .unwrap();

    let result = query(
        &server,
        "SELECT
            SUM(amount_cents * 0.01),
            AVG(amount_cents * 0.01),
            MIN(amount_cents * 0.01),
            MAX(amount_cents * 0.01),
            COUNT(DISTINCT amount_cents * 0.01)
         FROM measurements;",
    );

    assert_eq!(result.rows.len(), 1);

    for (column, sql) in [
        (0, "SELECT CAST(100.00 AS DECIMAL(18, 2));"),
        (1, "SELECT CAST(33.333333 AS DECIMAL(18, 6));"),
        (2, "SELECT CAST(30.00 AS DECIMAL(18, 2));"),
        (3, "SELECT CAST(40.00 AS DECIMAL(18, 2));"),
    ] {
        let Value::Decimal {
            value: actual_value,
            precision: actual_precision,
            scale: actual_scale,
        } = value(&result, 0, column)
        else {
            panic!("expected decimal aggregate at column {column}");
        };
        let expected = scalar(&server, sql);
        let Value::Decimal {
            value: expected_value,
            precision: expected_precision,
            scale: expected_scale,
        } = expected
        else {
            panic!("expected decimal scalar for {sql}");
        };

        assert_eq!(actual_value, &expected_value);
        assert_eq!(actual_precision, &expected_precision);
        assert_eq!(actual_scale, &expected_scale);
    }

    assert_eq!(value(&result, 0, 4), &Value::Int64(2));
}

#[test]
fn test_decimal_sum_reports_precision_overflow() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute(
            "CREATE TABLE balances (
                id BIGINT PRIMARY KEY,
                amount BIGINT
            );",
        )
        .unwrap();
    server
        .execute(
            "INSERT INTO balances (id, amount) VALUES
                (1, 999999999999999999),
                (2, 1);",
        )
        .unwrap();

    let error = server
        .execute("SELECT SUM(CAST(amount AS DECIMAL(18, 0))) FROM balances;")
        .expect_err("SUM beyond 18 digits must return an overflow error");

    assert!(
        error.to_string().to_lowercase().contains("out of range"),
        "expected out of range error, got: {error}"
    );
}

#[test]
fn test_decimal_window_sum_and_avg_match_grouped_aggregation() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute(
            "CREATE TABLE sales (
                id BIGINT PRIMARY KEY,
                region BIGINT,
                line_no BIGINT,
                amount_cents BIGINT
            );",
        )
        .unwrap();
    server
        .execute(
            "INSERT INTO sales (id, region, line_no, amount_cents) VALUES
                (1, 1, 1, 1000),
                (2, 1, 2, 2000),
                (3, 1, 3, 3000),
                (4, 2, 1, 500),
                (5, 2, 2, 1000);",
        )
        .unwrap();

    let grouped = query(
        &server,
        "SELECT
            region,
            SUM(amount_cents * 0.01),
            AVG(amount_cents * 0.01)
         FROM sales
         GROUP BY region
         ORDER BY region;",
    );
    let windowed = query(
        &server,
        "SELECT
            region,
            line_no,
            SUM(amount_cents * 0.01) OVER (PARTITION BY region),
            AVG(amount_cents * 0.01) OVER (PARTITION BY region)
         FROM sales
         ORDER BY region, line_no;",
    );

    assert_eq!(grouped.rows.len(), 2);
    assert_eq!(windowed.rows.len(), 5);

    for (grouped_row, windowed_row) in [(0, 0), (1, 3)] {
        assert_eq!(
            value(&grouped, grouped_row, 0),
            value(&windowed, windowed_row, 0)
        );

        for (grouped_column, windowed_column) in [(1, 2), (2, 3)] {
            let Value::Decimal {
                value: grouped_value,
                precision: grouped_precision,
                scale: grouped_scale,
            } = value(&grouped, grouped_row, grouped_column)
            else {
                panic!("expected decimal grouped aggregate");
            };
            let Value::Decimal {
                value: windowed_value,
                precision: windowed_precision,
                scale: windowed_scale,
            } = value(&windowed, windowed_row, windowed_column)
            else {
                panic!("expected decimal window aggregate");
            };

            assert_eq!(grouped_value, windowed_value);
            assert_eq!(grouped_precision, windowed_precision);
            assert_eq!(grouped_scale, windowed_scale);
        }
    }
}

#[test]
fn test_columnar_and_row_paths_agree_on_decimal_aggregation() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute(
            "CREATE TABLE revenue (
                id BIGINT PRIMARY KEY,
                category BIGINT,
                amount_cents BIGINT
            );",
        )
        .unwrap();
    server
        .execute(
            "INSERT INTO revenue (id, category, amount_cents) VALUES
                (1, 1, 1025),
                (2, 1, 2025),
                (3, 1, 3025),
                (4, 2, 110),
                (5, 2, 220);",
        )
        .unwrap();

    let sql = "SELECT
            category,
            SUM(amount_cents * 0.01),
            AVG(amount_cents * 0.01)
         FROM revenue
         GROUP BY category
         ORDER BY category;";
    let row_result = query(&server, sql);

    server.convert_table("revenue").unwrap();

    let columnar_result = query(&server, sql);

    assert_eq!(row_result.rows.len(), columnar_result.rows.len());
    for row in 0..row_result.rows.len() {
        assert_eq!(value(&row_result, row, 0), value(&columnar_result, row, 0));

        for column in [1, 2] {
            let Value::Decimal {
                value: row_value,
                precision: row_precision,
                scale: row_scale,
            } = value(&row_result, row, column)
            else {
                panic!("expected decimal row-store aggregate");
            };
            let Value::Decimal {
                value: columnar_value,
                precision: columnar_precision,
                scale: columnar_scale,
            } = value(&columnar_result, row, column)
            else {
                panic!("expected decimal columnar aggregate");
            };
            assert_eq!(row_value, columnar_value);
            assert_eq!(row_precision, columnar_precision);
            assert_eq!(row_scale, columnar_scale);
        }
    }
}

#[test]
fn test_decimal_to_double_assignment_and_union_with_integers() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute(
            "CREATE TABLE double_values (
                id BIGINT PRIMARY KEY,
                value DOUBLE
            );",
        )
        .unwrap();
    server
        .execute(
            "INSERT INTO double_values (id, value)
             VALUES (1, 12.34);",
        )
        .unwrap();

    let assigned_double = query(&server, "SELECT value FROM double_values;");
    assert_eq!(assigned_double.rows.len(), 1);
    assert_eq!(
        value(&assigned_double, 0, 0),
        &scalar(&server, "SELECT CAST(12.34 AS DOUBLE);")
    );

    let decimal_to_double = query(&server, "SELECT CAST(12.34 AS DOUBLE);");
    assert_eq!(decimal_to_double.rows.len(), 1);
    assert_eq!(
        value(&decimal_to_double, 0, 0),
        &scalar(&server, "SELECT CAST(12.34 AS DOUBLE);")
    );

    let union_result = query(
        &server,
        "SELECT amount
         FROM (
            SELECT 12.50 AS amount
            UNION ALL
            SELECT 7 AS amount
         ) AS amounts
         ORDER BY amount;",
    );

    assert_eq!(union_result.rows.len(), 2);

    for (row, sql) in [
        (0, "SELECT CAST(7.00 AS DECIMAL(18, 2));"),
        (1, "SELECT CAST(12.50 AS DECIMAL(18, 2));"),
    ] {
        let Value::Decimal {
            value: actual_value,
            precision: actual_precision,
            scale: actual_scale,
        } = value(&union_result, row, 0)
        else {
            panic!("expected decimal union result at row {row}");
        };
        let expected = scalar(&server, sql);
        let Value::Decimal {
            value: expected_value,
            precision: expected_precision,
            scale: expected_scale,
        } = expected
        else {
            panic!("expected decimal scalar for {sql}");
        };

        assert_eq!(actual_value, &expected_value);
        assert_eq!(actual_precision, &expected_precision);
        assert_eq!(actual_scale, &expected_scale);
    }
}
