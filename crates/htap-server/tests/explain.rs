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

fn create_join_tables(server: &LocalServer) {
    server
        .execute("CREATE TABLE left_input (id BIGINT PRIMARY KEY, value INT);")
        .unwrap();
    server
        .execute("CREATE TABLE right_input (id BIGINT PRIMARY KEY, value INT);")
        .unwrap();
    server
        .execute(
            "INSERT INTO left_input (id, value) VALUES
             (1, 10), (2, 20), (3, 30);",
        )
        .unwrap();
    server
        .execute(
            "INSERT INTO right_input (id, value) VALUES
             (1, 100), (2, 200), (4, 400), (5, 500);",
        )
        .unwrap();
}

#[test]
fn test_explain_primary_key_point_lookup_uses_rowstore_fast_path() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance INT);")
        .unwrap();
    server
        .execute("INSERT INTO accounts (id, balance) VALUES (1, 100);")
        .unwrap();

    let result = explain(
        &server,
        "EXPLAIN SELECT balance FROM accounts WHERE id = 1;",
    );
    let operation = column_index(&result, "operation");

    assert_eq!(result.rows.len(), 1);
    assert_eq!(
        string_value(&result.rows[0], operation),
        "RowstorePointRead"
    );
}

#[test]
fn test_explain_analytic_scan_uses_olap_fast_path() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE measurements (id BIGINT PRIMARY KEY, value INT);")
        .unwrap();
    server
        .execute(
            "INSERT INTO measurements (id, value) VALUES
             (1, 10), (2, 20), (3, 30);",
        )
        .unwrap();

    let result = explain(&server, "EXPLAIN SELECT value FROM measurements;");
    let operation = column_index(&result, "operation");

    assert_eq!(result.rows.len(), 1);
    assert_eq!(string_value(&result.rows[0], operation), "OlapScan");
}

#[test]
fn test_plain_explain_select_permitted_inside_open_transaction() {
    let dir = tempdir().unwrap();
    let server = std::sync::Arc::new(LocalServer::open(dir.path()).unwrap());
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, value INT);")
        .unwrap();
    server
        .execute("INSERT INTO t (id, value) VALUES (1, 10);")
        .unwrap();

    let mut session = server.open_session().unwrap();
    session.execute("BEGIN;").unwrap();
    session
        .execute("INSERT INTO t (id, value) VALUES (2, 20);")
        .unwrap();

    session
        .execute("EXPLAIN SELECT value FROM t WHERE id = 1;")
        .unwrap();
    assert!(
        session.in_transaction(),
        "EXPLAIN must not close the transaction"
    );

    session
        .execute("INSERT INTO t (id, value) VALUES (3, 30);")
        .unwrap();
    session.commit().unwrap();
}

#[test]
fn test_explain_join_estimates_report_default_then_stats_provenance() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    create_join_tables(&server);

    let sql = "EXPLAIN SELECT l.id, r.value
               FROM left_input l JOIN right_input r ON l.id = r.id;";
    let before = explain(&server, sql);
    let estimate_source = column_index(&before, "estimate_source");
    let estimated_rows = column_index(&before, "est_rows");

    let default_estimates: Vec<i64> = before
        .rows
        .iter()
        .filter(|row| !matches!(value(row, estimate_source), Value::Null))
        .map(|row| {
            assert_eq!(string_value(row, estimate_source), "Default");
            int_value(row, estimated_rows)
        })
        .collect();
    assert!(!default_estimates.is_empty());

    server.execute("ANALYZE TABLE left_input;").unwrap();
    server.execute("ANALYZE TABLE right_input;").unwrap();

    let after = explain(&server, sql);
    let stats_source = column_index(&after, "estimate_source");
    let stats_estimated_rows = column_index(&after, "est_rows");

    let stats_estimates: Vec<i64> = after
        .rows
        .iter()
        .filter(|row| !matches!(value(row, stats_source), Value::Null))
        .map(|row| {
            assert_eq!(string_value(row, stats_source), "Stats");
            int_value(row, stats_estimated_rows)
        })
        .collect();

    assert_eq!(stats_estimates.len(), default_estimates.len());
    assert_ne!(stats_estimates, default_estimates);
    assert!(stats_estimates.contains(&3));
    assert!(stats_estimates.contains(&4));
}

#[test]
fn test_explain_create_table_renders_plan_without_creating_table() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    let result = explain(&server, "EXPLAIN CREATE TABLE foo (id BIGINT PRIMARY KEY);");
    let operation = column_index(&result, "operation");

    assert!(result
        .rows
        .iter()
        .any(|row| { string_value(row, operation) == "CatalogDdl" }));
    assert!(
        server.execute("SELECT id FROM foo;").is_err(),
        "EXPLAIN must not create the table"
    );
}

#[test]
fn test_explain_analyze_create_table_executes_and_creates_table() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    let result = explain(
        &server,
        "EXPLAIN ANALYZE CREATE TABLE foo (id BIGINT PRIMARY KEY);",
    );
    let actual_rows = column_index(&result, "actual_rows");
    let actual_time_ms = column_index(&result, "actual_time_ms");

    assert!(result.rows.iter().any(|row| {
        !matches!(value(row, actual_rows), Value::Null)
            && !matches!(value(row, actual_time_ms), Value::Null)
    }));
    assert!(
        server.execute("SELECT id FROM foo;").is_ok(),
        "EXPLAIN ANALYZE must execute CREATE TABLE in autocommit mode"
    );
}

#[test]
fn test_analyze_statistics_survive_reopen() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE measurements (id BIGINT PRIMARY KEY, value INT);")
        .unwrap();
    server
        .execute("CREATE TABLE lookup_values (id BIGINT PRIMARY KEY, value INT);")
        .unwrap();
    server
        .execute(
            "INSERT INTO measurements (id, value) VALUES
             (1, 10), (2, 20), (3, 30);",
        )
        .unwrap();
    server
        .execute(
            "INSERT INTO lookup_values (id, value) VALUES
             (1, 100), (2, 200);",
        )
        .unwrap();
    server.execute("ANALYZE TABLE measurements;").unwrap();
    server.execute("ANALYZE TABLE lookup_values;").unwrap();
    drop(server);

    let reopened = LocalServer::open(dir.path()).unwrap();
    let result = explain(
        &reopened,
        "EXPLAIN SELECT m.value
         FROM measurements m
         JOIN lookup_values l ON m.id = l.id;",
    );
    let estimate_source = column_index(&result, "estimate_source");

    assert!(result.rows.iter().any(|row| {
        matches!(value(row, estimate_source), Value::String(source) if source == "Stats")
    }));
}

#[test]
fn test_explain_analyze_reports_root_actual_rows_and_time() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    create_join_tables(&server);

    let result = explain(
        &server,
        "EXPLAIN ANALYZE SELECT l.id, r.value
         FROM left_input l JOIN right_input r ON l.id = r.id;",
    );
    let node_id = column_index(&result, "node_id");
    let parent_id = column_index(&result, "parent_id");
    let actual_rows = column_index(&result, "actual_rows");
    let actual_time_ms = column_index(&result, "actual_time_ms");

    assert!(result
        .columns
        .iter()
        .any(|column| column.name == "est_rows"));
    assert!(result
        .columns
        .iter()
        .any(|column| column.name == "actual_rows"));
    assert!(result
        .columns
        .iter()
        .any(|column| column.name == "actual_time_ms"));

    let root = result
        .rows
        .iter()
        .find(|row| matches!(value(row, parent_id), Value::Null))
        .expect("plan has a root node");

    assert!(int_value(root, node_id) >= 0);
    assert!(!matches!(value(root, actual_rows), Value::Null));
    assert!(!matches!(value(root, actual_time_ms), Value::Null));
}

#[test]
/// This test validates that ANALYZE TABLE produces statistics-based row count
/// estimates in EXPLAIN output. Join reordering is not tested here; it depends
/// on the optimizer's cost model selection, which may result in the same order
/// being optimal with or without statistics.
fn test_explain_statistics_after_analyze_table() {
    let dir = tempdir().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    for table in ["t1", "t2", "t3"] {
        server
            .execute(&format!(
                "CREATE TABLE {table} (id BIGINT PRIMARY KEY, value INT);"
            ))
            .unwrap();
    }

    server
        .execute("INSERT INTO t1 (id, value) VALUES (1, 1);")
        .unwrap();

    let t2_values = (1..=10)
        .map(|id| format!("({id}, {id})"))
        .collect::<Vec<_>>()
        .join(", ");
    server
        .execute(&format!("INSERT INTO t2 (id, value) VALUES {t2_values};"))
        .unwrap();

    let t3_values = (1..=100)
        .map(|id| format!("({id}, {id})"))
        .collect::<Vec<_>>()
        .join(", ");
    server
        .execute(&format!("INSERT INTO t3 (id, value) VALUES {t3_values};"))
        .unwrap();

    let sql = "EXPLAIN SELECT t1.id, t2.value, t3.value
               FROM t1 JOIN t2 ON t1.id = t2.id
               JOIN t3 ON t2.id = t3.id;";

    // Return the unordered table pairs joined by the lowest join nodes.
    let lowest_join_pairs = |plan: &htap_sql::QueryResult| {
        let node_id = column_index(plan, "node_id");
        let parent_id = column_index(plan, "parent_id");
        let operation = column_index(plan, "operation");
        let table = column_index(plan, "table");

        let mut operations = std::collections::HashMap::new();
        let mut scan_tables = std::collections::HashMap::new();
        let mut children: std::collections::HashMap<i64, Vec<i64>> =
            std::collections::HashMap::new();

        for row in &plan.rows {
            let id = int_value(row, node_id);
            let operation_name = string_value(row, operation).to_owned();
            operations.insert(id, operation_name.clone());

            if operation_name == "TableScan" {
                scan_tables.insert(id, string_value(row, table).to_owned());
            }

            if let Value::Int64(parent) = value(row, parent_id) {
                children.entry(*parent).or_default().push(id);
            }
        }

        let join_nodes: Vec<i64> = operations
            .iter()
            .filter_map(|(id, operation)| operation.contains("Join").then_some(*id))
            .collect();

        let mut pairs = std::collections::BTreeSet::new();

        for join_id in join_nodes {
            let direct_children = children.get(&join_id).cloned().unwrap_or_default();
            if direct_children.iter().any(|child| {
                operations
                    .get(child)
                    .is_some_and(|operation| operation.contains("Join"))
            }) {
                continue;
            }

            let mut sides = Vec::new();
            for child in direct_children {
                let mut tables = std::collections::BTreeSet::new();
                let mut pending = vec![child];

                while let Some(node) = pending.pop() {
                    if let Some(table) = scan_tables.get(&node) {
                        tables.insert(table.clone());
                    }
                    if let Some(node_children) = children.get(&node) {
                        pending.extend(node_children.iter().copied());
                    }
                }

                sides.push(tables.into_iter().collect::<Vec<_>>());
            }

            assert_eq!(
                sides.len(),
                2,
                "lowest join node {join_id} should have two input sides"
            );
            sides.sort();
            pairs.insert((sides.remove(0), sides.remove(0)));
        }

        pairs
    };

    for table in ["t1", "t2", "t3"] {
        server.execute(&format!("ANALYZE TABLE {table};")).unwrap();
    }

    let after = explain(&server, sql);
    let after_lowest_pairs = lowest_join_pairs(&after);
    assert!(
        !after_lowest_pairs.is_empty(),
        "plan after ANALYZE should contain a lowest join node"
    );

    let operation = column_index(&after, "operation");
    let table = column_index(&after, "table");
    let estimate_source = column_index(&after, "estimate_source");
    let estimated_rows = column_index(&after, "est_rows");

    let scan_estimates: std::collections::HashMap<String, i64> = after
        .rows
        .iter()
        .filter(|row| string_value(row, operation) == "TableScan")
        .map(|row| {
            assert_eq!(string_value(row, estimate_source), "Stats");
            (
                string_value(row, table).to_owned(),
                int_value(row, estimated_rows),
            )
        })
        .collect();

    assert_eq!(scan_estimates.get("t1"), Some(&1));
    assert_eq!(scan_estimates.get("t2"), Some(&10));
    assert_eq!(scan_estimates.get("t3"), Some(&100));
}
