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

#[test]
fn test_decimal_primary_key_delete_non_representable_literal_matches_no_rows() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute("CREATE TABLE decimal_keys (pk DECIMAL(10,2) PRIMARY KEY, label VARCHAR);")
        .unwrap();
    server
        .execute("INSERT INTO decimal_keys (pk, label) VALUES (5.56, 'five-fifty-six');")
        .unwrap();

    server
        .execute("DELETE FROM decimal_keys WHERE pk = 5.555;")
        .expect("non-representable decimal primary key literal must match no rows");

    let result = query(&server, "SELECT pk FROM decimal_keys;");

    assert_eq!(result.rows.len(), 1);
    match value(&result.rows[0], 0) {
        Value::Decimal {
            value,
            precision,
            scale,
        } => {
            assert_eq!(*value, 556);
            assert_eq!(*precision, 10);
            assert_eq!(*scale, 2);
        }
        other => panic!("expected decimal at column 0, got {other:?}"),
    }
}

#[test]
fn test_decimal_primary_key_update_non_representable_literal_matches_no_rows() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute("CREATE TABLE decimal_keys (pk DECIMAL(10,2) PRIMARY KEY, label VARCHAR);")
        .unwrap();
    server
        .execute("INSERT INTO decimal_keys (pk, label) VALUES (5.56, 'five-fifty-six');")
        .unwrap();

    server
        .execute("UPDATE decimal_keys SET label = 'new-label' WHERE pk = 5.555;")
        .expect("non-representable decimal primary key literal must match no rows");

    let result = query(&server, "SELECT label FROM decimal_keys;");

    assert_eq!(result.rows.len(), 1);
    assert_eq!(
        value(&result.rows[0], 0),
        &Value::String("five-fifty-six".into())
    );
}

#[test]
fn test_decimal_assignment_rounds_non_representable_literal() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute("CREATE TABLE decimal_values (id INT PRIMARY KEY, value DECIMAL(10,2));")
        .unwrap();
    server
        .execute("INSERT INTO decimal_values (id, value) VALUES (1, 5.555);")
        .unwrap();

    let result = query(&server, "SELECT value FROM decimal_values;");

    assert_eq!(result.rows.len(), 1);
    match value(&result.rows[0], 0) {
        Value::Decimal {
            value,
            precision,
            scale,
        } => {
            assert_eq!(*value, 556);
            assert_eq!(*precision, 10);
            assert_eq!(*scale, 2);
        }
        other => panic!("expected decimal at column 0, got {other:?}"),
    }
}

#[test]
fn test_decimal_comparison_filter_non_representable_literal_rewrites_boundary() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute("CREATE TABLE decimal_values (id INT PRIMARY KEY, value DECIMAL(10,2));")
        .unwrap();
    server
        .execute("INSERT INTO decimal_values (id, value) VALUES (2, 5.56);")
        .unwrap();

    let greater = query(
        &server,
        "SELECT value FROM decimal_values WHERE value > 5.555;",
    );
    assert_eq!(greater.rows.len(), 1);
    match value(&greater.rows[0], 0) {
        Value::Decimal {
            value,
            precision,
            scale,
        } => {
            assert_eq!(*value, 556);
            assert_eq!(*precision, 10);
            assert_eq!(*scale, 2);
        }
        other => panic!("expected decimal at column 0, got {other:?}"),
    }
}

#[test]
fn test_decimal_partition_bound_rejects_non_representable_literal() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute(
            "CREATE TABLE decimal_partitioned (
                id INT,
                value DECIMAL(10,2),
                PRIMARY KEY (id, value)
            ) PARTITION BY RANGE (value) (
                PARTITION p0 VALUES LESS THAN (5.555)
            );",
        )
        .expect_err("non-representable decimal partition bound literal must be rejected");
}

#[test]
fn test_decimal_comparison_filter_accepts_exact_literal() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute("CREATE TABLE decimal_values (id INT PRIMARY KEY, value DECIMAL(10,2));")
        .unwrap();
    server
        .execute("INSERT INTO decimal_values (id, value) VALUES (3, 5.55);")
        .unwrap();

    let result = query(
        &server,
        "SELECT value FROM decimal_values WHERE value = 5.55;",
    );

    assert_eq!(result.rows.len(), 1);
    match value(&result.rows[0], 0) {
        Value::Decimal {
            value,
            precision,
            scale,
        } => {
            assert_eq!(*value, 555);
            assert_eq!(*precision, 10);
            assert_eq!(*scale, 2);
        }
        other => panic!("expected decimal at column 0, got {other:?}"),
    }
}

fn assert_decimal_rows(result: &htap_sql::QueryResult, expected: &[i64]) {
    assert_eq!(result.rows.len(), expected.len());

    for (row, expected_value) in result.rows.iter().zip(expected) {
        match value(row, 0) {
            Value::Decimal {
                value,
                precision,
                scale,
            } => {
                assert_eq!(*value, *expected_value);
                assert_eq!(*precision, 10);
                assert_eq!(*scale, 2);
            }
            other => panic!("expected decimal at column 0, got {other:?}"),
        }
    }
}

#[test]
fn test_decimal_less_than_non_representable_literal_uses_lower_neighbor() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute("CREATE TABLE decimal_values (id INT PRIMARY KEY, value DECIMAL(10,2));")
        .unwrap();
    server
        .execute("INSERT INTO decimal_values (id, value) VALUES (1, 5.55), (2, 5.56), (3, 5.57);")
        .unwrap();

    let result = query(
        &server,
        "SELECT value FROM decimal_values WHERE value < 5.555;",
    );

    assert_decimal_rows(&result, &[555i64]);
}

#[test]
fn test_decimal_less_than_or_equal_non_representable_literal_uses_upper_neighbor() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute("CREATE TABLE decimal_values (id INT PRIMARY KEY, value DECIMAL(10,2));")
        .unwrap();
    server
        .execute("INSERT INTO decimal_values (id, value) VALUES (1, 5.55), (2, 5.56), (3, 5.57);")
        .unwrap();

    let result = query(
        &server,
        "SELECT value FROM decimal_values WHERE value <= 5.555;",
    );

    assert_decimal_rows(&result, &[555i64]);
}

#[test]
fn test_decimal_equality_non_representable_literal_matches_no_rows() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute("CREATE TABLE decimal_values (id INT PRIMARY KEY, value DECIMAL(10,2));")
        .unwrap();
    server
        .execute("INSERT INTO decimal_values (id, value) VALUES (1, 5.55), (2, 5.56);")
        .unwrap();

    let result = query(
        &server,
        "SELECT value FROM decimal_values WHERE value = 5.555;",
    );

    assert_decimal_rows(&result, &[] as &[i64]);
}

#[test]
fn test_decimal_inequality_non_representable_literal_matches_all_non_null_rows() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute("CREATE TABLE decimal_values (id INT PRIMARY KEY, value DECIMAL(10,2));")
        .unwrap();
    server
        .execute("INSERT INTO decimal_values (id, value) VALUES (1, 5.55), (2, 5.56), (3, NULL);")
        .unwrap();

    let result = query(
        &server,
        "SELECT value FROM decimal_values WHERE value <> 5.555;",
    );

    assert_decimal_rows(&result, &[555i64, 556i64]);
}

#[test]
fn test_decimal_negative_non_representable_literal_rewrites_all_comparisons() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute("CREATE TABLE decimal_values (id INT PRIMARY KEY, value DECIMAL(10,2));")
        .unwrap();
    server
        .execute(
            "INSERT INTO decimal_values (id, value) VALUES (1, -5.57), (2, -5.56), (3, -5.55);",
        )
        .unwrap();

    assert_decimal_rows(
        &query(
            &server,
            "SELECT value FROM decimal_values WHERE value < -5.555;",
        ),
        &[-557i64, -556i64],
    );
    assert_decimal_rows(
        &query(
            &server,
            "SELECT value FROM decimal_values WHERE value <= -5.555;",
        ),
        &[-557i64, -556i64],
    );
    assert_decimal_rows(
        &query(
            &server,
            "SELECT value FROM decimal_values WHERE value > -5.555;",
        ),
        &[-555i64],
    );
    assert_decimal_rows(
        &query(
            &server,
            "SELECT value FROM decimal_values WHERE value >= -5.555;",
        ),
        &[-555i64],
    );
    assert_decimal_rows(
        &query(
            &server,
            "SELECT value FROM decimal_values WHERE value = -5.555;",
        ),
        &[],
    );
    assert_decimal_rows(
        &query(
            &server,
            "SELECT value FROM decimal_values WHERE value <> -5.555;",
        ),
        &[-557i64, -556i64, -555i64],
    );
}

#[test]
fn test_decimal_representable_literal_preserves_all_comparison_operators() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute("CREATE TABLE decimal_values (id INT PRIMARY KEY, value DECIMAL(10,2));")
        .unwrap();
    server
        .execute("INSERT INTO decimal_values (id, value) VALUES (1, 5.25), (2, 5.50), (3, 5.75);")
        .unwrap();

    assert_decimal_rows(
        &query(
            &server,
            "SELECT value FROM decimal_values WHERE value < 5.50;",
        ),
        &[525i64],
    );
    assert_decimal_rows(
        &query(
            &server,
            "SELECT value FROM decimal_values WHERE value <= 5.50;",
        ),
        &[525i64, 550i64],
    );
    assert_decimal_rows(
        &query(
            &server,
            "SELECT value FROM decimal_values WHERE value > 5.50;",
        ),
        &[575i64],
    );
    assert_decimal_rows(
        &query(
            &server,
            "SELECT value FROM decimal_values WHERE value >= 5.50;",
        ),
        &[550i64, 575i64],
    );
    assert_decimal_rows(
        &query(
            &server,
            "SELECT value FROM decimal_values WHERE value = 5.50;",
        ),
        &[550i64],
    );
    assert_decimal_rows(
        &query(
            &server,
            "SELECT value FROM decimal_values WHERE value <> 5.50;",
        ),
        &[525i64, 575i64],
    );
}

#[test]
fn test_decimal_add_range_partition_rejects_non_representable_literal() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute(
            "CREATE TABLE decimal_partitioned (
                id INT,
                value DECIMAL(10,2),
                PRIMARY KEY (id, value)
            ) PARTITION BY RANGE (value) (
                PARTITION p0 VALUES LESS THAN (5.55)
            );",
        )
        .unwrap();

    server
        .execute(
            "ALTER TABLE decimal_partitioned
             ADD PARTITION (PARTITION p1 VALUES LESS THAN (5.555));",
        )
        .expect_err("non-representable decimal range partition bound literal must be rejected");
}

#[test]
fn test_decimal_add_list_partition_rejects_non_representable_literal() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute(
            "CREATE TABLE decimal_partitioned (
                id INT,
                value DECIMAL(10,2),
                PRIMARY KEY (id, value)
            ) PARTITION BY LIST (value) (
                PARTITION p0 VALUES IN (5.55)
            );",
        )
        .unwrap();

    server
        .execute(
            "ALTER TABLE decimal_partitioned
             ADD PARTITION (PARTITION p1 VALUES IN (5.555));",
        )
        .expect_err("non-representable decimal list partition bound literal must be rejected");
}

#[test]
fn test_decimal_reorganize_range_partition_rejects_non_representable_literal() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute(
            "CREATE TABLE decimal_partitioned (
                id INT,
                value DECIMAL(10,2),
                PRIMARY KEY (id, value)
            ) PARTITION BY RANGE (value) (
                PARTITION p0 VALUES LESS THAN (5.55),
                PARTITION p1 VALUES LESS THAN (MAXVALUE)
            );",
        )
        .unwrap();

    server
        .execute(
            "ALTER TABLE decimal_partitioned
             REORGANIZE PARTITION p0 INTO (
                 PARTITION p0a VALUES LESS THAN (5.555),
                 PARTITION p0b VALUES LESS THAN (5.55)
             );",
        )
        .expect_err("non-representable decimal range partition bound literal must be rejected");
}

#[test]
fn test_decimal_reorganize_list_partition_rejects_non_representable_literal() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute(
            "CREATE TABLE decimal_partitioned (
                id INT,
                value DECIMAL(10,2),
                PRIMARY KEY (id, value)
            ) PARTITION BY LIST (value) (
                PARTITION p0 VALUES IN (5.55),
                PARTITION p1 VALUES IN (5.56)
            );",
        )
        .unwrap();

    server
        .execute(
            "ALTER TABLE decimal_partitioned
             REORGANIZE PARTITION p0 INTO (
                 PARTITION p0a VALUES IN (5.555)
             );",
        )
        .expect_err("non-representable decimal list partition bound literal must be rejected");
}
